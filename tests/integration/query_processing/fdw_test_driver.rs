//! An in-process foreign data wrapper whose rows a test can set directly.
//!
//! The CSV driver can express most mirror cases, but not the ones that turn on
//! what a driver is *allowed* to return — a NULL where it declared an identity,
//! say — because those go through a file and a parser that normalise them away.
//! This driver hands the engine exactly the values the test wrote.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use turso_core::foreign::{
    FdwChange, ForeignCursor, ForeignDataWrapper, KeyColumn, PushedConstraint, StreamingForeignData,
};
use turso_core::{Connection, Value};

/// The rows a [`MemFdw`] returns, shared with the test that owns it.
#[derive(Clone, Default)]
pub struct MemRows(Arc<Mutex<Vec<Vec<Value>>>>);

impl MemRows {
    pub fn set(&self, rows: Vec<Vec<Value>>) {
        *self.0.lock().unwrap() = rows;
    }

    fn get(&self) -> Vec<Vec<Value>> {
        self.0.lock().unwrap().clone()
    }
}

/// A foreign table backed by [`MemRows`].
///
/// It is also a streaming source: [`MemFdw::push`] both records the change in
/// the rows a rescan would return and emits it to the subscription, so a scan
/// and the change feed always tell the same story — which is what lets a test
/// assert that a `REFRESH` after a push finds nothing left to do.
#[derive(Debug)]
pub struct MemFdw {
    schema_sql: String,
    identity: Option<Vec<u32>>,
    rows: MemRows,
    changes: Sender<FdwChange>,
    subscription: Mutex<Option<Receiver<FdwChange>>>,
}

impl std::fmt::Debug for MemRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MemRows")
    }
}

impl MemFdw {
    /// A table declaring `identity` over the columns of `schema_sql`.
    pub fn new(schema_sql: &str, identity: Vec<u32>) -> (Arc<Self>, MemRows) {
        let rows = MemRows::default();
        let (changes, subscription) = channel();
        let fdw = Arc::new(Self {
            schema_sql: schema_sql.to_string(),
            identity: Some(identity),
            rows: rows.clone(),
            changes,
            subscription: Mutex::new(Some(subscription)),
        });
        (fdw, rows)
    }

    /// Announce a row arriving at the source. Weight +1 inserts or replaces,
    /// −1 removes.
    pub fn push(&self, change: FdwChange) {
        let identity = self.identity.clone().unwrap_or_default();
        let key = |row: &[Value]| -> Vec<Value> {
            identity.iter().map(|i| row[*i as usize].clone()).collect()
        };
        let target = key(&change.values);
        let mut rows = self.rows.get();
        rows.retain(|row| key(row) != target);
        if change.weight > 0 {
            rows.push(change.values.clone());
        }
        self.rows.set(rows);
        self.changes.send(change).unwrap();
    }
}

impl ForeignDataWrapper for MemFdw {
    fn key_columns(&self) -> &[KeyColumn] {
        &[]
    }

    fn identity_columns(&self) -> Option<&[u32]> {
        self.identity.as_deref()
    }

    fn schema_sql(&self) -> String {
        self.schema_sql.clone()
    }

    fn open_cursor(&self, _conn: Arc<Connection>) -> turso_core::Result<Box<dyn ForeignCursor>> {
        Ok(Box::new(MemCursor {
            rows: self.rows.get(),
            index: 0,
        }))
    }
}

impl StreamingForeignData for MemFdw {
    fn subscribe(
        &self,
        _constraints: &[PushedConstraint],
    ) -> turso_core::Result<Receiver<FdwChange>> {
        Ok(self
            .subscription
            .lock()
            .unwrap()
            .take()
            .expect("a MemFdw admits one subscriber"))
    }
}

struct MemCursor {
    rows: Vec<Vec<Value>>,
    index: usize,
}

impl ForeignCursor for MemCursor {
    fn filter(&mut self, _constraints: &[PushedConstraint]) -> turso_core::Result<bool> {
        self.index = 0;
        Ok(!self.rows.is_empty())
    }

    fn next(&mut self) -> turso_core::Result<bool> {
        self.index += 1;
        Ok(self.index < self.rows.len())
    }

    fn column(&self, idx: usize) -> turso_core::Result<Value> {
        Ok(self.rows[self.index]
            .get(idx)
            .cloned()
            .unwrap_or(Value::Null))
    }

    fn rowid(&self) -> i64 {
        self.index as i64
    }
}
