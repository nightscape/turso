use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::types::{
    CallbackId, ChangeCallbackEntry, ChangeCallbackFn, DatabaseChange, DatabaseChangeType,
    ImmutableRecord, RelationChangeEvent, RelationFilter,
};
use crate::vdbe::row_change::{normalize_table_change_record, RowChange};

pub(crate) struct StagedRowChange {
    pub(crate) change: RowChange,
    pub(crate) relation_name: String,
    pub(crate) is_materialized_view: bool,
}

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

pub(crate) fn dispatch(callbacks: &ChangeCallbacks, staged: Vec<StagedRowChange>) {
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

fn build_events(staged: Vec<StagedRowChange>) -> Vec<RelationChangeEvent> {
    let mut relations: Vec<RelationRows> = Vec::new();
    let mut relation_of_name: HashMap<String, usize> = HashMap::new();
    for staged_row in staged {
        let StagedRowChange {
            change: row,
            relation_name,
            is_materialized_view,
        } = staged_row;
        let relation = *relation_of_name
            .entry(relation_name.clone())
            .or_insert_with(|| {
                relations.push(RelationRows {
                    relation_name,
                    table: row.table.clone(),
                    is_materialized_view,
                    rows: Vec::new(),
                    row_of_rowid: HashMap::new(),
                });
                relations.len() - 1
            });
        let relation = &mut relations[relation];
        let image = |record: &ImmutableRecord| {
            if is_materialized_view {
                view_image(record)
            } else {
                table_image(&row.table, row.rowid, record)
            }
        };
        let before = row.images.before().map(image);
        let after = row.images.after().map(image);
        match relation.row_of_rowid.get(&row.rowid) {
            Some(&index) => relation.rows[index].after = after,
            None => {
                relation.row_of_rowid.insert(row.rowid, relation.rows.len());
                relation.rows.push(NetRow {
                    rowid: row.rowid,
                    before,
                    after,
                });
            }
        }
    }
    relations
        .into_iter()
        .filter_map(RelationRows::into_event)
        .collect()
}

struct RelationRows {
    relation_name: String,
    table: std::sync::Arc<crate::schema::BTreeTable>,
    is_materialized_view: bool,
    rows: Vec<NetRow>,
    row_of_rowid: HashMap<i64, usize>,
}

struct NetRow {
    rowid: i64,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

impl RelationRows {
    fn into_event(self) -> Option<RelationChangeEvent> {
        let changed: Vec<NetRow> = self
            .rows
            .into_iter()
            .filter(|row| row.before != row.after)
            .collect();
        let mut deletes = Vec::new();
        let mut updates = Vec::new();
        let mut inserts = Vec::new();
        for row in changed {
            match (row.before, row.after) {
                (Some(before), None) => {
                    deletes.push((row.rowid, DatabaseChangeType::Delete { bin_record: before }))
                }
                (None, Some(after)) => {
                    inserts.push((row.rowid, DatabaseChangeType::Insert { bin_record: after }))
                }
                (Some(before), Some(after)) if self.is_materialized_view => {
                    deletes.push((row.rowid, DatabaseChangeType::Delete { bin_record: before }));
                    inserts.push((row.rowid, DatabaseChangeType::Insert { bin_record: after }));
                }
                (Some(_), Some(after)) => {
                    updates.push((row.rowid, DatabaseChangeType::Update { bin_record: after }))
                }
                (None, None) => unreachable!("equal images were filtered out"),
            }
        }
        if deletes.is_empty() && updates.is_empty() && inserts.is_empty() {
            return None;
        }
        let relation_name = self.relation_name;
        let changes = deletes
            .into_iter()
            .chain(updates)
            .chain(inserts)
            .enumerate()
            .map(|(index, (id, change))| DatabaseChange {
                change_id: index as i64 + 1,
                change_time: 0,
                change,
                table_name: relation_name.clone(),
                id,
            })
            .collect();
        Some(RelationChangeEvent {
            columns: self
                .table
                .columns()
                .iter()
                .map(|column| column.name.clone().expect("a relation column has a name"))
                .collect(),
            changes,
            relation_name,
            commit_id: 0,
            commit_index: 0,
            commit_len: 0,
        })
    }
}

fn view_image(record: &ImmutableRecord) -> Vec<u8> {
    let mut values = record
        .get_values_owned()
        .expect("a staged view row image decodes");
    values.pop().expect("a view row image ends with its weight");
    encode(&values)
}

fn table_image(table: &crate::schema::BTreeTable, rowid: i64, record: &ImmutableRecord) -> Vec<u8> {
    let values = record
        .get_values_owned()
        .expect("a staged table row image decodes");
    let values = normalize_table_change_record(table, rowid, values)
        .expect("a staged table row image matches its table");
    encode(&values)
}

fn encode(values: &[crate::Value]) -> Vec<u8> {
    ImmutableRecord::from_values(values, values.len())
        .expect("a row image encodes")
        .get_payload()
        .to_vec()
}
