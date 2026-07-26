//! Leaf-boundary lookups in the DBSP state index.
//!
//! `SeekOp::GE { eq_only: true }` on an index btree reports `NotFound` for keys
//! that are present: when the leaf binary search finds nothing and no EQ was
//! seen in an interior node, `indexbtree_seek_inner` returns `NotFound` where
//! the non-`eq_only` path returns `TryAdvance` — and the matching entry can be
//! in the neighbouring leaf, which is exactly what `TryAdvance` exists to tell
//! the caller. `eq_only` callers are never given that chance, so they conclude
//! the row does not exist.
//!
//! This test covers the **write** site, `WriteRow::GetRecord`: it inserts 400
//! distinct keys with the production `WriteRow` and the production index
//! definition, then writes every key a second time. A lookup that misses turns
//! the second write into an insert, so the symptom is duplicate state rows —
//! silently, with no error.
//!
//! The **read** site (`AggregateEvalState::FetchKey`, which seeks the same
//! index) is covered end-to-end by `ivm_yield_matview_rowloss.rs`. The two are
//! genuinely independent: fixing only `WriteRow::GetRecord` turns this test
//! green while that one stays red.

use crate::incremental::operator::{create_dbsp_state_index, DbspStateCursors};
use crate::incremental::persistence::WriteRow;
use crate::storage::btree::{BTreeCursor, CursorTrait};
use crate::storage::pager::CreateBTreeFlags;
use crate::sync::Arc;
use crate::types::IOResult;
use crate::util::IOExt;
use crate::{Database, MemoryIO, SqliteDialect, Value, IO};

/// Same key shape the aggregate operator uses: operator id, 16-byte group hash,
/// 16-byte element id.
fn keys(n: usize) -> Vec<[u8; 16]> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0x243f_6a88_85a3_08d3;
    for _ in 0..n {
        let mut b = [0u8; 16];
        for chunk in b.chunks_mut(8) {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            chunk.copy_from_slice(&state.to_le_bytes());
        }
        out.push(b);
    }
    out
}

fn index_key(zset: &[u8; 16]) -> Vec<Value> {
    vec![
        Value::from_i64(131072),
        Value::from_slice(zset).unwrap(),
        Value::from_slice(&[0u8; 16]).unwrap(),
    ]
}

#[test]
fn eq_only_index_seek_finds_every_present_key() {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap();
    let conn = db.connect().unwrap();
    let pager = conn.pager.load().clone();
    let _ = pager.io.block(|| pager.allocate_page1());
    let table_root = pager
        .io
        .block(|| pager.btree_create(&CreateBTreeFlags::new_table()))
        .unwrap() as i64;
    let index_root = pager
        .io
        .block(|| pager.btree_create(&CreateBTreeFlags::new_index()))
        .unwrap() as i64;

    let table_cursor = BTreeCursor::new_table(pager.clone(), table_root, 5);
    let index_def = create_dbsp_state_index(index_root);
    let index_cursor = BTreeCursor::new_index(pager.clone(), index_root, &index_def, 3).unwrap();
    let mut cursors = DbspStateCursors::new(table_cursor, index_cursor);

    // Enough keys that the index btree has more than one leaf. One page of
    // entries never reproduces this.
    let keys = keys(400);
    for zset in &keys {
        let ikey = index_key(zset);
        let mut record_values = ikey.clone();
        record_values.push(Value::from_slice(&[7u8; 24]).unwrap());
        let mut wr = WriteRow::new();
        pager
            .io
            .block(|| wr.write_row(&mut cursors, ikey.clone(), record_values.clone(), 1))
            .unwrap();
    }

    let mut index_entries = 0;
    pager.io.block(|| cursors.index_cursor.rewind()).unwrap();
    loop {
        let present = pager
            .io
            .block(|| {
                Ok(match cursors.index_cursor.record()? {
                    IOResult::Done(r) => IOResult::Done(r.is_some()),
                    IOResult::IO(io) => IOResult::IO(io),
                })
            })
            .unwrap();
        if !present {
            break;
        }
        index_entries += 1;
        pager.io.block(|| cursors.index_cursor.next()).unwrap();
    }
    assert_eq!(index_entries, keys.len(), "index lost entries");

    // Second write of every key: a lookup that misses inserts a duplicate
    // instead of bumping the weight.
    for zset in &keys {
        let ikey = index_key(zset);
        let mut record_values = ikey.clone();
        record_values.push(Value::from_slice(&[7u8; 24]).unwrap());
        let mut wr = WriteRow::new();
        pager
            .io
            .block(|| wr.write_row(&mut cursors, ikey.clone(), record_values.clone(), 1))
            .unwrap();
    }

    let mut table_rows = 0;
    let mut wrong_weight = 0;
    pager.io.block(|| cursors.table_cursor.rewind()).unwrap();
    while pager
        .io
        .block(|| cursors.table_cursor.rowid())
        .unwrap()
        .is_some()
    {
        table_rows += 1;
        let weight = pager
            .io
            .block(|| {
                Ok(match cursors.table_cursor.record()? {
                    IOResult::Done(r) => IOResult::Done(
                        r.map(|r| r.get_value(4).unwrap().to_owned().unwrap())
                            .unwrap_or(Value::Null),
                    ),
                    IOResult::IO(io) => IOResult::IO(io),
                })
            })
            .unwrap();
        if weight != Value::from_i64(2) {
            wrong_weight += 1;
        }
        pager.io.block(|| cursors.table_cursor.next()).unwrap();
    }

    assert_eq!(
        table_rows,
        keys.len(),
        "duplicate DBSP state rows: WriteRow::GetRecord failed to find keys that are \
         present (leaf-boundary eq_only seek)"
    );
    assert_eq!(
        wrong_weight, 0,
        "{wrong_weight} state rows did not accumulate their second write"
    );
}
