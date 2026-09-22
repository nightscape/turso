//! Reading a secondary index on a materialized view.
//!
//! A view's btree, table and indexes alike, is written only when its delta is
//! applied at commit. Inside a transaction its rows come from a `ViewOverlay`,
//! the same one `MaterializedViewCursor` reads, so the table and its indexes
//! cannot disagree.

use std::cmp::Ordering;

use crate::incremental::cursor::ViewOverlay;
use crate::storage::btree::CursorTrait;
use crate::sync::Arc;
use crate::types::{
    compare_immutable, IOResult, IOResultOr, ImmutableRecord, IndexInfo, SeekKey, SeekOp,
    SeekResult, Value,
};
use crate::vdbe::Register;
use crate::{return_if_io, LimboError, Pager, Result};

/// One step of the committed index btree's contribution to the merge. Each
/// arm issues exactly one cursor op and advances only once it completes, so a
/// yield resumes where it left off.
#[derive(Debug, Clone, Copy)]
enum BtreeStep {
    /// `btree_pending` is settled; the merge may read it.
    Idle,
    Rewinding,
    Lasting,
    /// Seek to `seek_key`, whose first `seek_prefix` columns are significant.
    Seeking {
        op: SeekOp,
    },
    Advancing {
        forward: bool,
    },
    Reading {
        forward: bool,
    },
}

/// Which repositioning is in flight. A public entry point sets up its merge
/// state only when nothing is in flight, so re-entry after an IO yield
/// resumes instead of restarting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Settled,
    Positioning,
}

/// A cursor over an index on a materialized view: the committed index btree
/// merged with the entries the open transaction adds to and retracts from it.
///
/// Entries are `[key columns…, rowid]`, the layout of an ordinary secondary
/// index on a rowid table, and they compare under the index's own
/// `IndexInfo::key_info`.
pub struct MaterializedViewIndexCursor {
    btree_cursor: Box<dyn CursorTrait>,
    pager: Arc<Pager>,
    overlay: ViewOverlay,
    index_info: Arc<IndexInfo>,
    /// Positions in the view's output row that make up the key, in key order.
    key_positions: Vec<usize>,
    /// The overlay holds the complete view, so the committed btree is not read.
    btree_ignored: bool,

    /// Entries the overlay adds, and entries it retracts, both sorted under
    /// `index_info.key_info`. A retracted entry exists in the committed btree
    /// and must be skipped there.
    additions: Vec<Vec<Value>>,
    tombstones: Vec<Vec<Value>>,

    /// The next committed entry in the current direction, tombstones already
    /// skipped, or None past the end.
    btree_pending: Option<Vec<Value>>,
    btree_step: BtreeStep,
    seek_key: Vec<Value>,
    /// How many of `seek_key`'s columns the in-flight seek compares.
    seek_prefix: usize,
    phase: Phase,
    /// Forward: the next unconsumed addition. Backward: one past it.
    add_pos: usize,

    current: Option<Vec<Value>>,
    current_record: Option<ImmutableRecord>,
    null_flag: bool,

    count_total: Option<usize>,
    count_counted_current: bool,
}

impl MaterializedViewIndexCursor {
    pub fn new(
        btree_cursor: Box<dyn CursorTrait>,
        pager: Arc<Pager>,
        overlay: ViewOverlay,
        index_info: Arc<IndexInfo>,
        key_positions: Vec<usize>,
    ) -> Self {
        Self {
            btree_cursor,
            pager,
            overlay,
            index_info,
            key_positions,
            btree_ignored: false,
            additions: Vec::new(),
            tombstones: Vec::new(),
            btree_pending: None,
            btree_step: BtreeStep::Idle,
            seek_key: Vec::new(),
            seek_prefix: 0,
            phase: Phase::Settled,
            add_pos: 0,
            current: None,
            current_record: None,
            null_flag: false,
            count_total: None,
            count_counted_current: false,
        }
    }

    fn cmp_entries(&self, l: &[Value], r: &[Value]) -> Ordering {
        cmp_prefix_with(&self.index_info.key_info, l, r, usize::MAX)
    }

    fn cmp_prefix(&self, l: &[Value], r: &[Value], prefix: usize) -> Ordering {
        cmp_prefix_with(&self.index_info.key_info, l, r, prefix)
    }

    /// Project the view's overlay into index entries, split by sign.
    fn ensure_overlay_computed(&mut self) -> IOResultOr<()> {
        if !return_if_io!(self.overlay.ensure_computed()) {
            return Ok(IOResult::Done(()));
        }

        self.btree_ignored = self.overlay.full_result_mode;
        self.additions.clear();
        self.tombstones.clear();
        for (row, weight) in self.overlay.uncommitted.iter() {
            let mut entry = Vec::with_capacity(self.key_positions.len() + 1);
            for pos in &self.key_positions {
                entry.push(row.values.get(*pos).cloned().ok_or_else(|| {
                    LimboError::InternalError(format!(
                        "matview index key position {pos} out of range for a {}-column row",
                        row.values.len()
                    ))
                })?);
            }
            entry.push(Value::from_i64(row.rowid));
            if weight > 0 {
                self.additions.push(entry);
            } else {
                self.tombstones.push(entry);
            }
        }
        let key_info = self.index_info.key_info.clone();
        self.additions
            .sort_by(|a, b| cmp_prefix_with(&key_info, a, b, usize::MAX));
        self.tombstones
            .sort_by(|a, b| cmp_prefix_with(&key_info, a, b, usize::MAX));
        Ok(IOResult::Done(()))
    }

    fn is_tombstoned(&self, entry: &[Value]) -> bool {
        self.tombstones
            .binary_search_by(|t| self.cmp_entries(t, entry))
            .is_ok()
    }

    /// Drive the committed side until `btree_pending` is settled.
    fn drive_btree(&mut self) -> IOResultOr<()> {
        loop {
            match self.btree_step {
                BtreeStep::Idle => return Ok(IOResult::Done(())),
                BtreeStep::Rewinding | BtreeStep::Lasting | BtreeStep::Seeking { .. }
                    if self.btree_ignored =>
                {
                    self.btree_pending = None;
                    self.btree_step = BtreeStep::Idle;
                }
                BtreeStep::Rewinding => {
                    return_if_io!(self.btree_cursor.rewind());
                    self.btree_step = BtreeStep::Reading { forward: true };
                }
                BtreeStep::Lasting => {
                    return_if_io!(self.btree_cursor.last());
                    self.btree_step = BtreeStep::Reading { forward: false };
                }
                BtreeStep::Seeking { op } => {
                    let forward = matches!(op, SeekOp::GT | SeekOp::GE { .. });
                    let key = ImmutableRecord::from_values(&self.seek_key, self.seek_key.len())?;
                    let res = return_if_io!(self
                        .btree_cursor
                        .seek(SeekKey::IndexKey(key.as_record_ref()), op));
                    self.btree_step = match res {
                        SeekResult::Found => BtreeStep::Reading { forward },
                        SeekResult::TryAdvance => BtreeStep::Advancing { forward },
                        SeekResult::NotFound => {
                            self.btree_pending = None;
                            BtreeStep::Idle
                        }
                    };
                }
                BtreeStep::Advancing { forward } => {
                    if forward {
                        return_if_io!(self.btree_cursor.next());
                    } else {
                        return_if_io!(self.btree_cursor.prev());
                    }
                    self.btree_step = BtreeStep::Reading { forward };
                }
                BtreeStep::Reading { forward } => {
                    if !self.btree_cursor.has_record() {
                        self.btree_pending = None;
                        self.btree_step = BtreeStep::Idle;
                        continue;
                    }
                    let values = match return_if_io!(self.btree_cursor.record()) {
                        Some(record) => record.get_values_owned()?,
                        None => {
                            self.btree_pending = None;
                            self.btree_step = BtreeStep::Idle;
                            continue;
                        }
                    };
                    if self.is_tombstoned(&values) {
                        self.btree_step = BtreeStep::Advancing { forward };
                        continue;
                    }
                    self.btree_pending = Some(values);
                    self.btree_step = BtreeStep::Idle;
                }
            }
        }
    }

    /// Take the next entry of the merged stream from whichever side holds it,
    /// and leave that side needing a refill. Pure: the refill is driven by the
    /// next call into `drive_btree`.
    fn choose(&mut self, forward: bool) {
        let from_add = if forward {
            self.additions.get(self.add_pos)
        } else {
            self.add_pos
                .checked_sub(1)
                .and_then(|i| self.additions.get(i))
        };

        // Decide and take in one step, so the btree entry is moved out of the
        // same `Option` the decision was made on.
        let from_btree = match (self.btree_pending.take(), from_add) {
            (None, None) => {
                self.current = None;
                self.current_record = None;
                return;
            }
            (Some(b), None) => Some(b),
            (None, Some(_)) => None,
            (Some(b), Some(a)) => {
                let ord = self.cmp_entries(&b, a);
                let take = if forward {
                    ord != Ordering::Greater
                } else {
                    ord != Ordering::Less
                };
                if take {
                    Some(b)
                } else {
                    self.btree_pending = Some(b);
                    None
                }
            }
        };

        self.current = Some(if let Some(entry) = from_btree {
            self.btree_step = BtreeStep::Advancing { forward };
            entry
        } else if forward {
            let entry = self.additions[self.add_pos].clone();
            self.add_pos += 1;
            entry
        } else {
            self.add_pos -= 1;
            self.additions[self.add_pos].clone()
        });
        self.current_record = None;
    }

    fn advance(&mut self, forward: bool) -> IOResultOr<()> {
        return_if_io!(self.drive_btree());
        self.choose(forward);
        Ok(IOResult::Done(()))
    }

    /// Position the additions side at the first entry satisfying `op` against
    /// the first `prefix` columns of `self.seek_key`.
    fn seek_additions(&mut self, op: SeekOp, prefix: usize) {
        let key = std::mem::take(&mut self.seek_key);
        // `partition_point` gives the first index whose entry is NOT before
        // the key under the op's strictness; for a backward op that index is
        // one past the entry we want.
        let pos = match op {
            SeekOp::GE { .. } | SeekOp::LT => self
                .additions
                .partition_point(|a| self.cmp_prefix(a, &key, prefix) == Ordering::Less),
            SeekOp::GT | SeekOp::LE { .. } => self
                .additions
                .partition_point(|a| self.cmp_prefix(a, &key, prefix) != Ordering::Greater),
        };
        self.seek_key = key;
        self.add_pos = pos;
    }

    fn seek_values(&mut self, key: Vec<Value>, op: SeekOp) -> IOResultOr<SeekResult> {
        return_if_io!(self.ensure_overlay_computed());
        if self.phase == Phase::Settled {
            self.seek_prefix = key.len();
            self.seek_key = key;
            self.seek_additions(op, self.seek_prefix);
            self.btree_step = BtreeStep::Seeking { op };
            self.phase = Phase::Positioning;
        }
        return_if_io!(self.drive_btree());
        self.phase = Phase::Settled;

        let forward = matches!(op, SeekOp::GT | SeekOp::GE { .. });
        self.choose(forward);

        let Some(found) = self.current.as_ref() else {
            return Ok(IOResult::Done(SeekResult::NotFound));
        };
        let eq_only = matches!(
            op,
            SeekOp::GE { eq_only: true } | SeekOp::LE { eq_only: true }
        );
        if eq_only && self.cmp_prefix(found, &self.seek_key, self.seek_prefix) != Ordering::Equal {
            self.current = None;
            self.current_record = None;
            return Ok(IOResult::Done(SeekResult::NotFound));
        }
        Ok(IOResult::Done(SeekResult::Found))
    }

    /// The trailing rowid of the entry under the cursor. The value comes off
    /// the committed index btree, so a damaged or foreign btree is reported,
    /// not panicked on.
    fn rowid_of_current(&self) -> Result<Option<i64>> {
        if !self.index_info.has_rowid {
            return Ok(None);
        }
        let Some(entry) = self.current.as_ref() else {
            return Ok(None);
        };
        match entry.last() {
            Some(Value::Numeric(crate::numeric::Numeric::Integer(rowid))) => Ok(Some(*rowid)),
            other => Err(LimboError::Corrupt(format!(
                "index on materialized view: entry ends in {other:?}, not a rowid"
            ))),
        }
    }
}

impl CursorTrait for MaterializedViewIndexCursor {
    fn rewind(&mut self) -> IOResultOr<()> {
        return_if_io!(self.ensure_overlay_computed());
        if self.phase == Phase::Settled {
            self.add_pos = 0;
            self.btree_step = BtreeStep::Rewinding;
            self.phase = Phase::Positioning;
        }
        return_if_io!(self.drive_btree());
        self.phase = Phase::Settled;
        self.choose(true);
        Ok(IOResult::Done(()))
    }

    fn last(&mut self) -> IOResultOr<()> {
        return_if_io!(self.ensure_overlay_computed());
        if self.phase == Phase::Settled {
            self.add_pos = self.additions.len();
            self.btree_step = BtreeStep::Lasting;
            self.phase = Phase::Positioning;
        }
        return_if_io!(self.drive_btree());
        self.phase = Phase::Settled;
        self.choose(false);
        Ok(IOResult::Done(()))
    }

    fn next(&mut self) -> IOResultOr<()> {
        self.null_flag = false;
        self.advance(true)
    }

    fn prev(&mut self) -> IOResultOr<()> {
        self.null_flag = false;
        self.advance(false)
    }

    fn seek(&mut self, key: SeekKey<'_>, op: SeekOp) -> IOResultOr<SeekResult> {
        let SeekKey::IndexKey(record) = key else {
            return Err(LimboError::InternalError(
                "a materialized view's index can only be searched with an index key".to_string(),
            )
            .into());
        };
        let values = record.get_values_owned()?;
        self.null_flag = false;
        self.seek_values(values, op)
    }

    fn seek_unpacked(&mut self, registers: &[Register], op: SeekOp) -> IOResultOr<SeekResult> {
        let values = registers
            .iter()
            .map(|r| r.get_value().clone())
            .collect::<Vec<_>>();
        self.null_flag = false;
        self.seek_values(values, op)
    }

    fn record(&mut self) -> IOResultOr<Option<&ImmutableRecord>> {
        if self.null_flag {
            return Ok(IOResult::Done(None));
        }
        if self.current_record.is_none() {
            let Some(values) = self.current.as_ref() else {
                return Ok(IOResult::Done(None));
            };
            self.current_record = Some(ImmutableRecord::from_values(values, values.len())?);
        }
        Ok(IOResult::Done(self.current_record.as_ref()))
    }

    fn rowid(&mut self) -> IOResultOr<Option<i64>> {
        if self.null_flag {
            return Ok(IOResult::Done(None));
        }
        Ok(IOResult::Done(self.rowid_of_current()?))
    }

    fn count(&mut self) -> IOResultOr<usize> {
        if self.count_total.is_none() {
            return_if_io!(self.rewind());
            self.count_total = Some(0);
            self.count_counted_current = false;
        }
        while self.current.is_some() {
            if !self.count_counted_current {
                self.count_total = Some(self.count_total.unwrap_or(0) + 1);
                self.count_counted_current = true;
            }
            return_if_io!(self.advance(true));
            self.count_counted_current = false;
        }
        Ok(IOResult::Done(self.count_total.take().unwrap_or(0)))
    }

    fn set_null_flag(&mut self, flag: bool) {
        self.null_flag = flag;
    }

    fn get_null_flag(&self) -> bool {
        self.null_flag
    }

    fn is_empty(&self) -> bool {
        self.current.is_none()
    }

    fn has_record(&self) -> bool {
        self.current.is_some()
    }

    fn set_has_record(&mut self, has_record: bool) {
        if !has_record {
            self.current = None;
            self.current_record = None;
        }
    }

    fn get_index_info(&self) -> &Arc<IndexInfo> {
        &self.index_info
    }

    fn has_rowid(&self) -> bool {
        self.index_info.has_rowid
    }

    fn root_page(&self) -> i64 {
        self.btree_cursor.root_page()
    }

    fn get_pager(&self) -> Arc<Pager> {
        self.pager.clone()
    }

    fn get_skip_advance(&self) -> bool {
        false
    }

    fn invalidate_record(&mut self) {
        self.current_record = None;
    }

    fn seek_end(&mut self) -> IOResultOr<()> {
        self.last()
    }

    fn seek_to_last(&mut self) -> IOResultOr<()> {
        self.last()
    }

    fn insert(&mut self, _: &crate::storage::btree::BTreeKey) -> IOResultOr<()> {
        Err(write_through_the_overlay("insert").into())
    }

    fn delete(&mut self) -> IOResultOr<()> {
        Err(write_through_the_overlay("delete").into())
    }

    fn clear_btree(&mut self) -> IOResultOr<Option<usize>> {
        Err(write_through_the_overlay("clear").into())
    }

    fn btree_destroy(&mut self) -> IOResultOr<Option<usize>> {
        Err(write_through_the_overlay("destroy").into())
    }

    fn exists(&mut self, _: &Value) -> IOResultOr<bool> {
        Err(LimboError::InternalError(
            "a materialized view's index has no rowid lookup".to_string(),
        )
        .into())
    }
}

/// Index entries compare under the index's own key info, over the first
/// `prefix` columns — a partial seek key is significant only as far as it goes.
fn cmp_prefix_with(
    key_info: &[crate::types::KeyInfo],
    l: &[Value],
    r: &[Value],
    prefix: usize,
) -> Ordering {
    let prefix = prefix.min(key_info.len()).min(l.len()).min(r.len());
    compare_immutable(
        l.iter().take(prefix).cloned(),
        r.iter().take(prefix).cloned(),
        &key_info[..prefix],
    )
}

/// A matview's index btree is written only by delta application, through a
/// plain b-tree cursor.
fn write_through_the_overlay(op: &str) -> LimboError {
    LimboError::InternalError(format!(
        "cannot {op} through a materialized view's index cursor"
    ))
}

impl std::fmt::Debug for MaterializedViewIndexCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaterializedViewIndexCursor")
            .field("additions", &self.additions.len())
            .field("tombstones", &self.tombstones.len())
            .field("btree_step", &self.btree_step)
            .field("add_pos", &self.add_pos)
            .finish()
    }
}
