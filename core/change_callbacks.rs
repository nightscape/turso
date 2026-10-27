use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::types::{
    CallbackId, ChangeCallbackEntry, ChangeCallbackFn, DatabaseChange, DatabaseChangeType,
    ImmutableRecord, RelationChangeEvent, RelationFilter,
};
use crate::vdbe::row_change::RowChange;

pub(crate) struct ChangeCallbacks {
    entries: parking_lot::RwLock<Vec<ChangeCallbackEntry>>,
    any: AtomicBool,
}

impl ChangeCallbacks {
    pub(crate) fn new() -> Self {
        Self {
            entries: parking_lot::RwLock::new(Vec::new()),
            any: AtomicBool::new(false),
        }
    }

    pub(crate) fn any(&self) -> bool {
        self.any.load(Ordering::Relaxed)
    }

    pub(crate) fn add(&self, filter: RelationFilter, callback: ChangeCallbackFn) -> CallbackId {
        let id = CallbackId::next();
        let mut entries = self.entries.write();
        entries.push((id, filter, callback));
        self.any.store(true, Ordering::Relaxed);
        id
    }

    pub(crate) fn remove(&self, id: CallbackId) {
        let mut entries = self.entries.write();
        entries.retain(|(callback_id, _, _)| *callback_id != id);
        self.any.store(!entries.is_empty(), Ordering::Relaxed);
    }

    pub(crate) fn clear(&self) {
        let mut entries = self.entries.write();
        entries.clear();
        self.any.store(false, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Vec<(RelationFilter, ChangeCallbackFn)> {
        self.entries
            .read()
            .iter()
            .map(|(_, filter, callback)| (filter.clone(), callback.clone()))
            .collect()
    }
}

pub(crate) fn dispatch(callbacks: &ChangeCallbacks, staged: Vec<RowChange>) {
    let mut events = build_events(staged);
    if events.is_empty() {
        return;
    }
    let callbacks = callbacks.snapshot();
    let commit_id = next_commit_id();
    let commit_len = events.len() as u32;
    for (index, event) in events.iter_mut().enumerate() {
        event.commit_id = commit_id;
        event.commit_index = index as u32;
        event.commit_len = commit_len;
    }
    for event in &events {
        for (filter, callback) in &callbacks {
            let wanted = filter
                .as_ref()
                .is_none_or(|relations| relations.contains(&event.relation_name));
            if !wanted {
                continue;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(event)));
            if let Err(panic) = result {
                tracing::error!(
                    "change callback panicked for relation '{}': {:?}",
                    event.relation_name,
                    panic
                );
            }
        }
    }
}

fn next_commit_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn build_events(staged: Vec<RowChange>) -> Vec<RelationChangeEvent> {
    let mut events: Vec<RelationChangeEvent> = Vec::new();
    let mut event_of_relation: HashMap<String, usize> = HashMap::new();
    for row in staged {
        let index = *event_of_relation
            .entry(row.table.name.clone())
            .or_insert_with(|| {
                events.push(RelationChangeEvent {
                    relation_name: row.table.name.clone(),
                    columns: row
                        .table
                        .columns()
                        .iter()
                        .map(|column| column.name.clone().expect("view column has a name"))
                        .collect(),
                    changes: Vec::new(),
                    commit_id: 0,
                    commit_index: 0,
                    commit_len: 0,
                });
                events.len() - 1
            });
        let event = &mut events[index];
        let deleted = row.images.before().map(|image| DatabaseChangeType::Delete {
            bin_record: view_image(image),
        });
        let inserted = row.images.after().map(|image| DatabaseChangeType::Insert {
            bin_record: view_image(image),
        });
        for change in deleted.into_iter().chain(inserted) {
            event.changes.push(DatabaseChange {
                change_id: event.changes.len() as i64 + 1,
                change_time: 0,
                change,
                table_name: event.relation_name.clone(),
                id: row.rowid,
            });
        }
    }
    events
}

pub(crate) fn view_image(record: &ImmutableRecord) -> Vec<u8> {
    let mut values = record
        .get_values_owned()
        .expect("a staged view row image decodes");
    values.pop().expect("a view row image ends with its weight");
    ImmutableRecord::from_values(&values, values.len())
        .expect("a view row image encodes")
        .get_payload()
        .to_vec()
}
