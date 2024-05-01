//! This module provides [ClientEngine] and [ServerEngine] that
//! implements the client and server part of the OT control algorithm.

use crate::error::{CollabError, CollabResult};
use crate::{op::quatradic_transform, types::*};
use serde::{Deserialize, Serialize};
use std::cmp::{max, min};
use std::collections::HashMap;
use std::result::Result;
use thiserror::Error;

// *** Data structures

/// A continuous series of local ops with a context. Used by clients
/// to send multiple ops to the server at once. The context sequence
/// represents the context on which the first local op in the series
/// is based: all the global ops with a sequence number leq to the
/// context seq number. The rest local ops' context are inferred by
/// their position in the vector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextOps {
    pub context: GlobalSeq,
    pub ops: Vec<FatOp>,
}

impl ContextOps {
    /// Return the site of the ops.
    pub fn site(&self) -> SiteId {
        self.ops[0].site()
    }
    /// Return the doc of the ops.
    pub fn doc(&self) -> DocId {
        self.ops[0].doc.clone()
    }
}

// *** InternalDoc

/// A range in the internal document. Each range is either a live text
/// (not deleted) or dead text (deleted). All the ranges should be in
/// order, connect, and don't overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Range {
    /// Live text, (len).
    Live(u64),
    /// Tombstone, (len).
    Dead(u64),
}

impl Range {
    /// Return true for live text, false for dead text.
    fn is_live(&self) -> bool {
        match self {
            Self::Live(_) => true,
            Self::Dead(_) => false,
        }
    }
    /// Return false for live text, true for dead text.
    fn is_dead(&self) -> bool {
        !self.is_live()
    }
    /// Return the length of the range.
    fn len(&self) -> u64 {
        match self {
            Self::Live(len) => *len,
            Self::Dead(len) => *len,
        }
    }

    /// Return the length of this range. If this range is dead, return
    /// 0.
    fn live_len(&self) -> u64 {
        if self.is_live() {
            self.len()
        } else {
            0
        }
    }
}

/// A cursor in the internal document (which contains tombstones).
/// This cursor also tracks the editor document's position.
/// Translating between editor position and internal document position
/// around a cursor is very fast.
#[derive(Debug, Clone, Copy)]
struct Cursor {
    /// The position of this cursor in the editor's document. This
    /// position doesn't account for the tombstones.
    editor_pos: u64,
    /// The actual position in the internal document containing
    /// tombstones.
    internal_pos: u64,
    /// The index of the range in which this cursor is. range.start <=
    /// cursor.internal_pos < range.end.
    range_idx: Option<usize>,
}

impl Cursor {
    // Return the editor_pos if `editor_pos` is true; return
    // internal_pos otherwise.
    fn pos(&self, editor_pos: bool) -> u64 {
        if editor_pos {
            self.editor_pos
        } else {
            self.internal_pos
        }
    }

    /// Move cursor to the beginning of `iter`.
    fn move_to_beg(&mut self, iter: &RangeIterator) {
        self.internal_pos = iter.range_beg;
        self.editor_pos = iter.editor_range_beg;
        self.range_idx = Some(iter.range_idx);
    }

    /// Move cursor to the position of `cursor`.
    fn move_to_cursor(&mut self, cursor: &Cursor) {
        self.range_idx = cursor.range_idx;
        self.internal_pos = cursor.internal_pos;
        self.editor_pos = cursor.editor_pos;
    }

    /// Shift the editor position and range idx.
    fn shift(&mut self, editor_pos_delta: i64, range_idx_delta: isize) {
        if editor_pos_delta > 0 {
            self.editor_pos += editor_pos_delta.abs() as u64;
        } else {
            self.editor_pos -= editor_pos_delta.abs() as u64;
        }
        if let Some(range_idx) = self.range_idx {
            if range_idx_delta > 0 {
                self.range_idx = Some(range_idx + range_idx_delta.abs() as usize)
            } else {
                self.range_idx = Some(range_idx - range_idx_delta.abs() as usize)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct RangeIterator {
    range_idx: usize,
    range_beg: u64,
    range_end: u64,
    is_live: bool,
    editor_range_beg: u64,
    editor_range_end: u64,
    affected_ranges_idx_beg: usize,
    affected_ranges_idx_end: usize,
}

impl RangeIterator {
    fn new(ranges: &[Range], cursor: &Cursor) -> RangeIterator {
        let range_idx = cursor.range_idx.unwrap();
        let range = &ranges[range_idx];
        let editor_range_len = if range.is_live() { range.len() } else { 0 };
        RangeIterator {
            range_idx,
            range_beg: cursor.internal_pos,
            range_end: cursor.internal_pos + range.len(),
            is_live: range.is_live(),
            affected_ranges_idx_beg: range_idx,
            editor_range_beg: cursor.editor_pos,
            editor_range_end: cursor.editor_pos + editor_range_len,
            affected_ranges_idx_end: range_idx + 1,
        }
    }

    /// Return the length of the current range.
    fn range_len(&self) -> u64 {
        self.range_end - self.range_beg
    }

    fn shift_affected_range_left(&mut self) {
        self.affected_ranges_idx_beg -= 1;
        self.affected_ranges_idx_end -= 1;
    }

    fn extend_affected_range_right(&mut self) {
        self.affected_ranges_idx_end += 1;
    }

    fn contract_affected_range_right(&mut self) {
        self.affected_ranges_idx_beg += 1;
    }

    /// Return the current range.
    fn range<'a>(&self, ranges: &'a [Range]) -> &'a Range {
        &ranges[self.range_idx]
    }

    /// Move to the left by one, return the currently range.
    fn move_left<'a>(&mut self, ranges: &'a [Range]) -> &'a Range {
        self.range_idx -= 1;
        let range = &ranges[self.range_idx];
        self.is_live = range.is_live();
        self.range_end = self.range_beg;
        self.range_beg = self.range_end - range.len();

        let editor_range_len = if range.is_live() { range.len() } else { 0 };
        self.editor_range_end = self.editor_range_beg;
        self.editor_range_beg = self.editor_range_end - editor_range_len;
        range
    }

    /// Move to the right by one, return the current range.
    fn move_right<'a>(&mut self, ranges: &'a [Range]) -> &'a Range {
        self.range_idx += 1;
        let range = &ranges[self.range_idx];
        self.is_live = range.is_live();
        self.range_beg = self.range_end;
        self.range_end = self.range_beg + range.len();

        let editor_range_len = if range.is_live() { range.len() } else { 0 };
        self.editor_range_beg = self.editor_range_end;
        self.editor_range_end = self.editor_range_beg + editor_range_len;
        range
    }
}

/// The internal full document that contains both live text and
/// tombstones. This is used to convert between editor positions (no
/// tombstone) and internal positions (includes tombstone).
#[derive(Debug, Clone)]
struct InternalDoc {
    /// Ranges fully covering the document. The ranges are coalesced
    /// after every insertion and deletion, so there should never be
    /// two consecutive ranges that has the same liveness.
    ranges: Vec<Range>,
    /// Cursor for each site. TODO: cursors garbage collect?
    cursors: HashMap<SiteId, Cursor>,
}

impl InternalDoc {
    /// Create a internal doc with initial document length at
    /// `init_len`.
    fn new(init_len: u64) -> InternalDoc {
        InternalDoc {
            ranges: if init_len > 0 {
                vec![Range::Live(init_len)]
            } else {
                vec![]
            },
            cursors: HashMap::new(),
        }
    }

    /// Check that the position of each cursor is correct.
    /// Only use this for debugging.
    fn check_cursor_positions(&self) {
        for (site, cursor) in self.cursors.iter() {
            if cursor.range_idx.is_none() {
                continue;
            }
            let mut editor_pos = 0;
            let mut internal_pos = 0;
            for range in self.ranges.iter() {
                if internal_pos == cursor.internal_pos {
                    if editor_pos != cursor.editor_pos {
                        log::error!("Cursor has wrong editor position, should be {}, cursor={:?}, site={site}, ranges={:?}",
                            editor_pos,
                        cursor, self.ranges);
                        std::process::exit(-1);
                    }
                    break;
                }
                match range {
                    Range::Live(len) => {
                        editor_pos += len;
                        internal_pos += len;
                    }
                    Range::Dead(len) => {
                        internal_pos += len;
                    }
                }
            }
        }
    }

    /// Return the total length of the editor document. Note that this
    /// function don't use any optimization and goes through every
    /// range front-to-back.
    fn editor_len(&self) -> u64 {
        let mut len = 0;
        for range in self.ranges.iter() {
            if let Range::Live(range_len) = range {
                len += *range_len;
            }
        }
        len
    }

    /// Return a copy of the cursor for `site`.
    fn get_cursor(&mut self, site: &SiteId) -> Cursor {
        if self.cursors.get(site).is_none() {
            let cursor = Cursor {
                editor_pos: 0,
                internal_pos: 0,
                range_idx: None,
            };
            self.cursors.insert(site.clone(), cursor);
        }
        self.cursors.get(site).unwrap().clone()
    }

    /// Convert a editor pos to a internal doc pos if `editor_pos` is
    /// true, and the other way around if false. This also moves the
    /// cursor to around editor pos. Unless there is no ranges in the
    /// doc, `cursor` must points to a range after this function
    /// returns (unless there's no range).
    fn convert_pos(&self, pos: u64, cursor: &mut Cursor, editor_pos: bool) -> u64 {
        if pos == 0 {
            // `cursor` must point to a range after this function
            // returns.
            if self.ranges.len() > 0 {
                cursor.internal_pos = 0;
                cursor.editor_pos = 0;
                cursor.range_idx = Some(0);
            }
            return 0;
        }
        if pos == cursor.pos(editor_pos) {
            return cursor.pos(!editor_pos);
        }
        if self.ranges.len() == 0 {
            return 0;
        }
        let mut range_idx = cursor.range_idx.unwrap_or(0);
        if range_idx == 0 {
            cursor.editor_pos = 0;
            cursor.internal_pos = 0;
        }
        let mut range = &self.ranges[range_idx];

        // At this point, `cursor` is at the beginning of `range`.
        // This should not change.

        // We go forward/backward until `editor_op` lies within
        // `range` (range.beg <= editor_op <= range.end).
        if pos > cursor.pos(editor_pos) {
            while pos
                > cursor.pos(editor_pos)
                    + if editor_pos {
                        range.live_len()
                    } else {
                        range.len()
                    }
            {
                cursor.internal_pos += range.len();
                if range.is_live() {
                    cursor.editor_pos += range.len();
                }
                range_idx += 1;
                cursor.range_idx = Some(range_idx);
                range = &self.ranges[range_idx];
            }
        } else {
            while (pos < cursor.pos(editor_pos))
                // To make editor pos -> internal pos is not
                // deterministic, we always pick the beg of a dead
                // range over the end of one.
                || (editor_pos && pos == cursor.pos(editor_pos) && range.is_live() && range_idx > 0)
            {
                range_idx -= 1;
                range = &self.ranges[range_idx];
                cursor.range_idx = Some(range_idx);

                cursor.internal_pos -= range.len();
                if range.is_live() {
                    cursor.editor_pos -= range.len();
                }
            }
        }
        // `cursor` must points to a range after this function
        // returns.
        cursor.range_idx = Some(range_idx);
        // At this point, `pos` should be within `range`. And
        // `cursor` is at the beginning of `range`.
        if editor_pos {
            // Editor pos -> internal pos.
            cursor.pos(!editor_pos) + (pos - cursor.pos(editor_pos))
        } else {
            // Internal pos -> editor pos.
            cursor.pos(!editor_pos)
                + if range.is_live() {
                    pos - cursor.pos(editor_pos)
                } else {
                    0
                }
        }
    }

    /// Shift all the cursors whose range_idx is equal or after `idx`
    /// by `delta` to the right, and shift their range_idx by
    /// `range_idx_delta` to the right.
    fn shift_cursors_right(&mut self, idx: usize, delta: u64, range_idx_delta: usize) {
        for (_site, cursor) in self.cursors.iter_mut() {
            if let Some(cursor_idx) = cursor.range_idx {
                if cursor_idx >= idx {
                    cursor.internal_pos += delta;
                    cursor.editor_pos += delta;
                    cursor.range_idx = Some(cursor_idx + range_idx_delta);
                }
            }
        }
    }

    /// Apply an insertion as internal position `pos` of `len`
    /// characters. `cursor` should point to the range that receives
    /// the insertion.
    fn apply_ins(&mut self, pos: u64, len: u64, cursor: &mut Cursor) {
        if self.ranges.len() == 0 {
            // (a)
            self.ranges.push(Range::Live(len));
            return;
        }
        let range_idx = cursor.range_idx.unwrap();
        let range = &mut self.ranges[range_idx];
        let range_beg = cursor.internal_pos;
        let range_end = range_beg + range.len();

        assert!(range_beg <= pos);
        assert!(pos <= range_end);
        if range.is_live() {
            // (b)
            *range = Range::Live(range.len() + len);
            self.shift_cursors_right(range_idx + 1, len, 0);
            return;
        }
        // `pos` at beginning of a dead range.
        if range_beg == pos {
            // Is there a range before this range?
            if range_idx >= 1 {
                // (c)
                let range_before = &mut self.ranges[range_idx - 1];
                assert!(range_before.is_live());
                *range_before = Range::Live(range_before.len() + len);
                self.shift_cursors_right(range_idx, len, 0);
                cursor.internal_pos += len;
                cursor.editor_pos += len;
            } else {
                // (d)
                self.ranges.insert(range_idx, Range::Live(len));
                self.shift_cursors_right(range_idx, len, 1);
                cursor.internal_pos += len;
                cursor.editor_pos += len;
                cursor.range_idx = Some(range_idx + 1);
            }
            return;
        }
        // `pos` at the end of a dead range.
        if range_end == pos {
            if range_idx + 1 < self.ranges.len() {
                // (e)
                let range_after = &mut self.ranges[range_idx + 1];
                assert!(range_after.is_live());
                *range_after = Range::Live(range_after.len() + len);
                self.shift_cursors_right(range_idx + 2, len, 0);
            } else {
                // (f)
                self.ranges.push(Range::Live(len));
            }
            return;
        }
        // (g) `pos` inside a dead range.
        let mid = pos - range_beg;
        if let Range::Dead(dead_len) = range {
            //    |                 range                   |
            // -> |  left_half  |   middle   |  right_half  |
            //                  ^
            //                  cursor
            let left_half = Range::Dead(mid);
            let right_half = Range::Dead(*dead_len - mid);
            let middle = Range::Live(len);
            self.ranges.remove(range_idx);
            self.ranges.insert(range_idx, right_half);
            self.ranges.insert(range_idx, middle);
            self.ranges.insert(range_idx, left_half);
            self.shift_cursors_right(range_idx + 1, len, 2);
            cursor.internal_pos = pos;
            cursor.editor_pos = cursor.editor_pos; // Don't change.
            cursor.range_idx = Some(range_idx + 1);
        } else {
            panic!();
        }
    }

    /// Mark the range from `pos` to `pos` + `len` as dead. `cursor`
    /// should point to the first [Range] in the range. Return the
    /// affected ranges that was actually flipped from live ranges, in
    /// the form of [(pos, len)].
    fn apply_mark_dead(&mut self, pos: u64, mut len: u64, cursor: &mut Cursor) -> Vec<(u64, u64)> {
        let marked_range_initial_beg = pos;
        let marked_range_initial_end = pos + len;

        let mut iter = RangeIterator::new(&self.ranges, cursor);
        let mut range = iter.range(&self.ranges);
        let mut final_ranges = vec![];
        let mut flipped_ranges = vec![];
        let mut flipped_ranges_len = 0;

        let mut push_wo_dup = |range: (u64, u64)| {
            if let Some(last_range) = flipped_ranges.last() {
                let (lbeg, llen) = *last_range;
                let lend = lbeg + llen;
                let (beg, len) = range;
                let end = beg + len;
                assert!(!(beg <= lend && lbeg <= end));
            }
            flipped_ranges.push(range);
            flipped_ranges_len += range.1;
        };

        // Ensure that the cursor is at the right range.
        assert!(iter.range_beg <= marked_range_initial_beg);

        // Move left one range so we can cover the range that precedes
        // the starting range.
        if iter.range_end > marked_range_initial_beg && iter.range_idx > 0 {
            range = iter.move_left(&self.ranges);
            iter.shift_affected_range_left();
        }

        loop {
            // [ range ]
            //           [  marked range  ]
            if iter.range_end < marked_range_initial_beg {
                iter.contract_affected_range_right();
            }
            // [ range ]
            //          [  marked range  ]
            else if iter.range_end == marked_range_initial_beg {
                if range.is_dead() {
                    len += iter.range_len();
                    // When we extend marked range left, we need to
                    // move cursor.
                    cursor.move_to_beg(&iter);
                } else {
                    iter.contract_affected_range_right();
                }
            }
            // At this point, current iter range must have some
            // overlap with the marked range.
            else {
                // [ range ]
                //    [      marked range     ]
                // or
                // [           range              ]
                //    [      marked range     ]
                // or
                //  [           range           ]
                //        [     marked range    ]
                if iter.range_beg < marked_range_initial_beg {
                    let prefix_len = marked_range_initial_beg - iter.range_beg;
                    if range.is_dead() {
                        // Extend the marked range.
                        len += prefix_len;
                        // When we extend marked range left, we need to
                        // move cursor.
                        cursor.move_to_beg(&iter);
                    } else {
                        let prefix_range = Range::Live(prefix_len);
                        final_ranges.push(prefix_range);

                        let flipped_beg = marked_range_initial_beg;
                        let flipped_end = min(marked_range_initial_end, iter.range_end);
                        push_wo_dup((flipped_beg, flipped_end - flipped_beg));
                    }
                }
                //                        [ range ]
                //    [      marked range     ]
                // or
                //  [           range           ]
                //    [      marked range     ]
                // or
                //  [           range           ]
                //  [     marked range    ]
                if iter.range_end > marked_range_initial_end {
                    let suffix_len = iter.range_end - marked_range_initial_end;
                    if range.is_dead() {
                        // Extend the marked range.
                        len += suffix_len;
                        final_ranges.push(Range::Dead(len));
                        break;
                    } else {
                        final_ranges.push(Range::Dead(len));
                        final_ranges.push(Range::Live(suffix_len));

                        if !(iter.range_beg < marked_range_initial_beg) {
                            let flipped_beg = max(iter.range_beg, marked_range_initial_beg);
                            let flipped_len = marked_range_initial_end - flipped_beg;
                            push_wo_dup((flipped_beg, flipped_len));
                        }

                        break;
                    }
                }
                //  [           range         ]
                //        [    marked range   ]
                // or
                //    [         range         ]
                //    [      marked range     ]
                // or
                //                [   range   ]
                //    [      marked range     ]
                if iter.range_end == marked_range_initial_end {
                    if range.is_live() {
                        if !(iter.range_beg < marked_range_initial_beg) {
                            let flipped_range = (iter.range_beg, iter.range_len());
                            push_wo_dup(flipped_range);
                        }
                    }
                    if iter.range_idx + 1 < self.ranges.len() {
                        let next_range = iter.move_right(&self.ranges);
                        if next_range.is_dead() {
                            len += iter.range_len();
                            iter.extend_affected_range_right();
                        }
                    }
                    final_ranges.push(Range::Dead(len));
                    break;
                }
                //            [ range ]
                //    [      marked range     ]
                // or
                //    [ range ]
                //    [      marked range     ]
                // or
                //  [  range  ]
                //    [      marked range     ]
                // Do nothing and go to next iteration.
                if range.is_live() {
                    if !(iter.range_beg < marked_range_initial_beg) {
                        push_wo_dup((iter.range_beg, iter.range_len()));
                    }
                }
            }

            range = iter.move_right(&self.ranges);
            iter.extend_affected_range_right();
            assert!(iter.range_idx < self.ranges.len());
        }
        // Loop ends.

        // At this point, `final_ranges` contains 1-3 ranges that are
        // going to replace the affected ranges.
        let affected_ranges_count = iter.affected_ranges_idx_end - iter.affected_ranges_idx_beg;
        let range_idx_delta = (final_ranges.len() as isize) - (affected_ranges_count as isize);
        let editor_pos_delta = -(flipped_ranges_len as i64);

        self.ranges.splice(
            iter.affected_ranges_idx_beg..iter.affected_ranges_idx_end,
            final_ranges,
        );
        for (_, other_cursor) in self.cursors.iter_mut() {
            if let Some(other_cursor_range_idx) = other_cursor.range_idx {
                if other_cursor_range_idx >= iter.affected_ranges_idx_beg
                    && other_cursor_range_idx < iter.affected_ranges_idx_end
                {
                    other_cursor.move_to_cursor(cursor);
                } else if other_cursor_range_idx >= iter.affected_ranges_idx_end {
                    other_cursor.shift(editor_pos_delta, range_idx_delta);
                }
            }
        }
        flipped_ranges
    }

    /// Mark the range from `pos` to `pos` + `len` as live. `cursor`
    /// should point to the first [Range] in the range.
    fn apply_mark_live(&mut self, pos: u64, mut len: u64, cursor: &mut Cursor) {
        let marked_range_initial_beg = pos;
        let marked_range_initial_end = pos + len;

        let mut iter = RangeIterator::new(&self.ranges, cursor);
        let mut range = iter.range(&self.ranges);
        let mut final_ranges = vec![];

        let mut flipped_ranges = vec![];
        let mut flipped_ranges_len = 0;

        let mut push_wo_dup = |range: (u64, u64)| {
            if let Some(last_range) = flipped_ranges.last() {
                let (lbeg, llen) = *last_range;
                let lend = lbeg + llen;
                let (beg, len) = range;
                let end = beg + len;
                assert!(!(beg <= lend && lbeg <= end));
            }
            flipped_ranges.push(range);
            flipped_ranges_len += range.1;
        };

        // Ensure that the cursor is at the right range.
        assert!(iter.range_beg <= marked_range_initial_beg);

        // Move left one range so we can cover the range that precedes
        // the starting range.
        if iter.range_end > marked_range_initial_beg && iter.range_idx > 0 {
            range = iter.move_left(&self.ranges);
            iter.shift_affected_range_left();
        }

        loop {
            // [ range ]
            //           [  marked range  ]
            if iter.range_end < marked_range_initial_beg {
                iter.contract_affected_range_right();
            }
            // [ range ]
            //          [  marked range  ]
            else if iter.range_end == marked_range_initial_beg {
                if range.is_live() {
                    len += iter.range_len();
                    // When we extend marked range left, we need to
                    // move cursor.
                    cursor.move_to_beg(&iter);
                } else {
                    iter.contract_affected_range_right();
                }
            }
            // At this point, current iter range must have some
            // overlap with the marked range.
            else {
                // [ range ]
                //    [      marked range     ]
                // or
                // [           range              ]
                //    [      marked range     ]
                if iter.range_beg < marked_range_initial_beg {
                    let prefix_len = marked_range_initial_beg - iter.range_beg;
                    if range.is_dead() {
                        let prefix_range = Range::Dead(prefix_len);
                        final_ranges.push(prefix_range);

                        let flipped_beg = marked_range_initial_beg;
                        let flipped_end = min(marked_range_initial_end, iter.range_end);
                        push_wo_dup((flipped_beg, flipped_end - flipped_beg));
                    } else {
                        // Extend the marked range.
                        len += prefix_len;
                        // When we extend marked range left, we need
                        // to move cursor.
                        cursor.move_to_beg(&iter);
                    }
                }
                //                        [ range ]
                //    [      marked range     ]
                // or
                //  [           range           ]
                //    [      marked range     ]
                if iter.range_end > marked_range_initial_end {
                    let suffix_mid = marked_range_initial_end - iter.range_beg;
                    if let Range::Dead(range_len) = range {
                        let suffix_range = Range::Dead(range_len - suffix_mid);
                        final_ranges.push(Range::Live(len));
                        final_ranges.push(suffix_range);

                        if !(iter.range_beg < marked_range_initial_beg) {
                            let flipped_beg = max(iter.range_beg, marked_range_initial_beg);
                            let flipped_len = marked_range_initial_end - flipped_beg;
                            push_wo_dup((flipped_beg, flipped_len));
                        }
                        break;
                    } else {
                        // Extend the marked range.
                        len += range.len() - suffix_mid;
                        final_ranges.push(Range::Live(len));
                        break;
                    }
                }
                //                    [ range ]
                //    [      marked range     ]
                if iter.range_end == marked_range_initial_end {
                    if range.is_dead() {
                        if !(iter.range_beg < marked_range_initial_beg) {
                            let flipped_range = (iter.range_beg, iter.range_len());
                            push_wo_dup(flipped_range);
                        }
                    }
                    if iter.range_idx + 1 < self.ranges.len() {
                        let next_range = iter.move_right(&self.ranges);
                        if next_range.is_live() {
                            len += iter.range_len();
                            iter.extend_affected_range_right();
                        }
                    }
                    final_ranges.push(Range::Live(len));
                    break;
                }
                //            [ range ]
                //    [      marked range     ]
                // or
                //    [ range ]
                //    [      marked range     ]
                // Do nothing and go to next iteration.
                if range.is_dead() {
                    if !(iter.range_beg < marked_range_initial_beg) {
                        push_wo_dup((iter.range_beg, iter.range_len()));
                    }
                }
            }

            range = iter.move_right(&self.ranges);
            iter.extend_affected_range_right();
            assert!(iter.range_idx < self.ranges.len());
        }
        // Loop over.

        // At this point, `final_ranges` contains 1-3 ranges that are
        // going to replace the affected ranges.
        let affected_ranges_count = iter.affected_ranges_idx_end - iter.affected_ranges_idx_beg;
        let range_idx_delta = (final_ranges.len() as isize) - (affected_ranges_count as isize);
        let editor_pos_delta = flipped_ranges_len as i64;

        self.ranges.splice(
            iter.affected_ranges_idx_beg..iter.affected_ranges_idx_end,
            final_ranges,
        );
        for (_, other_cursor) in self.cursors.iter_mut() {
            if let Some(other_cursor_range_idx) = other_cursor.range_idx {
                if other_cursor_range_idx >= iter.affected_ranges_idx_beg
                    && other_cursor_range_idx < iter.affected_ranges_idx_end
                {
                    other_cursor.move_to_cursor(cursor);
                } else if other_cursor_range_idx >= iter.affected_ranges_idx_end {
                    other_cursor.shift(editor_pos_delta, range_idx_delta);
                }
            }
        }
    }

    /// If `live` is true, convert mark_live at `pos` with `text` to a
    /// ins op for the editor; if false, convert mark_dead at `pos`
    /// with `text` to a del op for the editor. `cursor` should point
    /// to the first [Range] in the affected range. Push the converted
    /// ranges into `ranges`.
    fn mark_to_ins_or_del(
        &self,
        pos: u64,
        text: String,
        cursor: &Cursor,
        ranges: &mut Vec<(u64, String)>,
        live: bool,
    ) {
        if text.len() == 0 {
            return;
        }
        let marked_beg = pos;
        let marked_end = pos + text.chars().count() as u64;
        let mut iter = RangeIterator::new(&self.ranges, cursor);

        loop {
            if (live != iter.is_live) && iter.range_beg <= marked_beg && marked_beg < iter.range_end
            {
                // [  range  ]      or   [  range  ]
                //   [   mrkd   ]            [mrkd]
                //   [  cap  ]               [cap ]
                let captured_start = 0;
                let captured_end = (min(iter.range_end, marked_end) - marked_beg) as usize;
                // let captured_text = text[captured_start..captured_end].to_string();
                let captured_text = text
                    .chars()
                    .skip(captured_start)
                    .take(captured_end - captured_start)
                    .collect::<String>();
                let captured_editor_beg = if iter.is_live {
                    iter.editor_range_beg + (marked_beg - iter.range_beg)
                } else {
                    iter.editor_range_beg
                };
                ranges.push((captured_editor_beg, captured_text));
            } else if (live != iter.is_live)
                && iter.range_beg <= marked_end
                && marked_end < iter.range_end
            {
                //    [   range   ]
                // [  marked   ]
                //    [  cap   ]
                let captured_start = (max(iter.range_beg, marked_beg) - marked_beg) as usize;
                let captured_end = text.chars().count();
                // let captured_text = text[captured_start..captured_end].to_string();
                let captured_text = text
                    .chars()
                    .skip(captured_start)
                    .take(captured_end - captured_start)
                    .collect::<String>();
                let captured_editor_beg = iter.editor_range_beg;
                ranges.push((captured_editor_beg, captured_text));
            } else if (live != iter.is_live)
                && iter.range_beg > marked_beg
                && iter.range_end < marked_end
            {
                //    [  range  ]
                // [     marked    ]
                let captured_start = (iter.range_beg - marked_beg) as usize;
                let captured_end = (iter.range_end - marked_beg) as usize;
                // let captured_text = text[captured_start..captured_end].to_string();
                let captured_text = text
                    .chars()
                    .skip(captured_start)
                    .take(captured_end - captured_start)
                    .collect::<String>();
                let captured_editor_beg = iter.editor_range_beg;
                ranges.push((captured_editor_beg, captured_text));
            }

            // Prepare states for the next iteration.
            if iter.range_idx + 1 == self.ranges.len() {
                return;
            }
            iter.move_right(&self.ranges);
            if iter.range_beg >= marked_end {
                return;
            }
        }
    }

    // Convert the position of `op` to internal doc position, and
    // apply it to the internal doc.
    fn convert_editor_op_and_apply(&mut self, op: EditorOp, site: SiteId) -> Op {
        log::debug!("convert_editor_op_and_apply(op={:?})", &op);
        log::debug!("ranges_before={:?}", &self.ranges);

        let mut cursor = self.get_cursor(&site);
        let converted_op = match op.clone() {
            EditorOp::Ins(pos, text) => {
                let internal_pos = self.convert_pos(pos, &mut cursor, true);
                self.apply_ins(internal_pos, text.chars().count() as u64, &mut cursor);
                self.cursors.insert(site, cursor);
                Op::Ins((internal_pos, text), site)
            }
            EditorOp::Del(pos, text) => {
                let internal_pos = self.convert_pos(pos, &mut cursor, true);
                let editor_end = pos + text.chars().count() as u64;
                let internal_len =
                    self.convert_pos(editor_end, &mut cursor.clone(), true) - internal_pos;
                // Make sure to skip the already dead ranges. Then
                // when we undo this op, we only insert the text we
                // actually deleted.
                let flipped_ranges = self.apply_mark_dead(internal_pos, internal_len, &mut cursor);
                let mut text_idx = 0;
                let mut converted_ops = vec![];
                for (pos, len) in flipped_ranges {
                    let len = len as usize;
                    // converted_ops.push((pos, text[text_idx..text_idx + len].to_string()));
                    converted_ops.push((pos, text.chars().skip(text_idx).take(len).collect()));
                    text_idx += len;
                }
                self.cursors.insert(site.clone(), cursor);
                Op::Mark(converted_ops, false, site)
            }
            _ => panic!(),
        };

        log::debug!("ranges_after={:?}", &self.ranges,);
        log::debug!("converted_op={:?}", &converted_op);

        // Put it back.
        self.cursors.insert(site, cursor);
        self.check_cursor_positions();
        converted_op
    }

    /// Convert internal `op` from internal pos to editor pos, and
    /// apply it to the internal_doc.
    fn convert_internal_op_and_apply(&mut self, op: Op) -> EditInstruction {
        log::debug!("convert_internal_op_and_apply(op={:?})", &op);
        log::debug!("ranges_before={:?}", &self.ranges);
        log::debug!("cursors={:?}", &self.cursors);

        let site = op.site();
        let mut cursor = self.get_cursor(&site);
        let instr = match op {
            Op::Ins((pos, text), _) => {
                let editor_pos = self.convert_pos(pos, &mut cursor, false);
                self.apply_ins(pos, text.chars().count() as u64, &mut cursor);
                self.cursors.insert(site.clone(), cursor);
                EditInstruction::Ins(vec![(editor_pos, text)])
            }
            Op::Mark(edits, live, _) => {
                let mut new_edits = vec![];
                let mut new_edits_bare = vec![];
                for (pos, text) in edits.into_iter() {
                    new_edits_bare.push((pos, text.chars().count() as u64));
                    // Move cursor to the right position.
                    self.convert_pos(pos, &mut cursor, false);
                    self.mark_to_ins_or_del(pos, text, &cursor, &mut new_edits, live);
                }
                for (pos, len) in new_edits_bare.into_iter() {
                    self.convert_pos(pos, &mut cursor, false);
                    if live {
                        self.apply_mark_live(pos, len, &mut cursor);
                    } else {
                        self.apply_mark_dead(pos, len, &mut cursor);
                    }
                }
                self.cursors.insert(site.clone(), cursor);
                if live {
                    EditInstruction::Ins(new_edits)
                } else {
                    EditInstruction::Del(new_edits)
                }
            }
        };
        self.check_cursor_positions();

        log::debug!("ranges_after={:?}", &self.ranges,);
        log::debug!("converted_op={:?}", &instr);

        instr
    }
}

// *** GlobalHistory

/// Global history buffer. The whole buffer is made of global_buffer +
/// local_buffer.
#[derive(Debug, Clone)]
struct GlobalHistory {
    /// The global ops section.
    global: Vec<FatOp>,
    /// The local ops section.
    local: Vec<FatOp>,
    /// A vector of pointer to the locally generated ops in the
    /// history. These ops are what the editor can undo. We use global
    /// seq as pointers, but these pointers can also refer to ops in
    /// the `local` history which don't have global seq yet. Their
    /// global seq are inferred to be length of global history + their
    /// index in local history. Obviously these inferred indexes need
    /// to be updated every time we receive a remote op.
    undo_queue: Vec<GlobalSeq>,
    /// An index into `local_ops`. Points to the currently undone op.
    undo_tip: Option<usize>,
}

impl GlobalHistory {
    /// Create a global history where `init_len` is the initial length
    /// of the document it will be tracking.
    fn new() -> GlobalHistory {
        GlobalHistory {
            global: vec![],
            local: vec![],
            undo_queue: vec![],
            undo_tip: None,
        }
    }

    /// Get the global ops with sequence number larger than `seq`.
    fn ops_after(&self, seq: GlobalSeq) -> Vec<FatOp> {
        if seq < self.global.len() as GlobalSeq {
            self.global[(seq as usize)..].to_vec()
        } else {
            vec![]
        }
    }

    /// Return the global and local ops after `seq`.
    pub fn all_ops_after(&self, seq: GlobalSeq) -> Vec<FatOp> {
        let mut ops = vec![];
        if seq < self.global.len() as GlobalSeq {
            ops.extend_from_slice(&self.global[(seq as usize)..]);
            ops.extend_from_slice(&self.local[..]);
        } else {
            ops.extend_from_slice(&self.local[(seq as usize) - self.global.len()..]);
        }
        ops
    }

    /// Return the op referenced by the `idxidx`th pointer in the undo
    /// queue.
    fn nth_in_undo_queue(&self, idxidx: usize) -> &FatOp {
        let seq = self.undo_queue[idxidx] as usize;
        let idx = seq - 1;
        if idx < self.global.len() {
            &self.global[idx]
        } else {
            &self.local[idx - self.global.len()]
        }
    }

    /// Process a local op according to its kind, return a suitable
    /// OpKind for the op. `op_seq` is the inferred global seq for the
    /// local op.
    pub fn process_opkind(
        &mut self,
        kind: EditorOpKind,
        op_seq: GlobalSeq,
    ) -> EngineResult<OpKind> {
        let seq_of_new_op = self.global.len() as usize + self.local.len() + 1 + 1;
        match kind {
            EditorOpKind::Original => {
                // If the editor undone an op and makes an original
                // edit, the undone op is lost (can't be redone
                // anymore).
                if let Some(idx) = self.undo_tip {
                    self.undo_queue.drain(idx..);
                }
                self.undo_tip = None;
                self.undo_queue.push(op_seq);
                Ok(OpKind::Original)
            }
            EditorOpKind::Undo => {
                // If this op is an undo op, find the original op it's
                // supposed to undo, and calculate the delta.
                let idx_of_orig;
                if let Some(idx) = self.undo_tip {
                    if idx == 0 {
                        return Err(EngineError::UndoError("No more op to undo".to_string()));
                    }
                    idx_of_orig = self.undo_queue[idx - 1];
                    self.undo_tip = Some(idx - 1);
                } else {
                    if self.undo_queue.len() == 0 {
                        return Err(EngineError::UndoError("No more op to undo".to_string()));
                    }
                    idx_of_orig = *self.undo_queue.last().unwrap();
                    self.undo_tip = Some(self.undo_queue.len() - 1);
                }
                // Next redo will undo this undo op.
                self.undo_queue[self.undo_tip.unwrap()] = op_seq;
                let seq_of_orig = idx_of_orig + 1;
                Ok(OpKind::Undo(seq_of_new_op - seq_of_orig as usize))
            }
            EditorOpKind::Redo => {
                if let Some(idx) = self.undo_tip {
                    let idx_of_inverse = self.undo_queue[idx];
                    if idx + 1 == self.undo_queue.len() {
                        self.undo_tip = None;
                    } else {
                        self.undo_tip = Some(idx + 1);
                    }
                    // Next undo will undo this redo op.
                    self.undo_queue[idx] = op_seq;
                    let seq_of_inverse = idx_of_inverse + 1;
                    Ok(OpKind::Undo(seq_of_new_op - seq_of_inverse as usize))
                } else {
                    Err(EngineError::UndoError("No ops to redo".to_string()))
                }
            }
        }
    }

    /// Print debugging history.
    pub fn print_history(&self) -> String {
        fn print_kind(kind: OpKind) -> &'static str {
            match kind {
                OpKind::Original => "O",
                OpKind::Undo(_) => "U",
            }
        }

        let mut output = String::new();
        output += "SEQ\tGROUP\tKIND\tOP\n\n";
        output += "ACKED:\n\n";
        for op in &self.global {
            output += format!(
                "{}\t{}\t{\t}\t{}\n",
                op.seq.unwrap(),
                op.group_seq,
                print_kind(op.kind),
                op.op
            )
            .as_str();
        }
        output += "\nUNACKED:\n\n";
        for op in &self.local {
            output += format!(
                " \t{}\t{\t}\t{}\n",
                op.group_seq,
                print_kind(op.kind),
                op.op
            )
            .as_str();
        }
        output += "\nUNDO QUEUE:\n\n";
        for idx in 0..self.undo_queue.len() {
            let op = self.nth_in_undo_queue(idx);
            output += format!("{}\t{}\n", self.undo_queue[idx], op.op).as_str();
        }
        if let Some(tip) = self.undo_tip {
            output += format!("\nUNDO TIP: {}", self.undo_queue[tip]).as_str();
        } else {
            output += "\nUNDO TIP: EMPTY";
        }

        output
    }

    pub fn print_history_debug(&self) -> String {
        fn print_kind(kind: OpKind, seq: GlobalSeq) -> String {
            match kind {
                OpKind::Original => "O".to_string(),
                OpKind::Undo(delta) => format!("U({})", seq - delta as u32),
            }
        }

        let mut output = String::new();
        output += "SEQ\tGROUP\tKIND\tOP\n\n";
        output += "ACKED:\n\n";
        for op in &self.global {
            output += format!(
                "{}\t{}\t{\t}\t{:?}\n",
                op.seq.unwrap(),
                op.group_seq,
                print_kind(op.kind, op.seq.unwrap()),
                op.op
            )
            .as_str();
        }
        output += "\nUNACKED:\n\n";
        let mut seq = self.global.len();
        for op in &self.local {
            output += format!(
                " \t{}\t{\t}\t{:?}\n",
                op.group_seq,
                print_kind(op.kind, seq as GlobalSeq),
                op.op
            )
            .as_str();
            seq += 1;
        }
        output += "\nUNDO QUEUE:\n\n";
        for idx in 0..self.undo_queue.len() {
            let op = self.nth_in_undo_queue(idx);
            output += format!("{}\t{:?}\n", self.undo_queue[idx], op.op).as_str();
        }
        if let Some(tip) = self.undo_tip {
            output += format!("\nUNDO TIP: {}", self.undo_queue[tip]).as_str();
        } else {
            output += "\nUNDO TIP: EMPTY";
        }

        output
    }
}

// *** Client and server engine

/// OT control algorithm engine for client. Used by
/// [crate::collab_doc::Doc].
#[derive(Debug, Clone)]
pub struct ClientEngine {
    /// History storing the global timeline (seen by the server).
    gh: GlobalHistory,
    /// The internal document.
    internal_doc: InternalDoc,
    /// The id of this site.
    site: SiteId,
    /// The largest global sequence number we've seen. Any remote op
    /// we receive should have sequence equal to this number plus one.
    current_seq: GlobalSeq,
    /// The largest local sequence number we've seen. Any local op we
    /// receive should be have local sequence equal to this number plus
    /// one.
    current_site_seq: LocalSeq,
    /// When we send local `ops` to server, record the largest site
    /// seq in `ops`. Before server acked the op with this site seq,
    /// we can't send more local ops to server (stop-and-wait).
    last_site_seq_sent_out: LocalSeq,
}

/// OT control algorithm engine for server. Used by
/// [crate::collab_server::LocalServer].
#[derive(Debug, Clone)]
pub struct ServerEngine {
    /// Global history.
    gh: GlobalHistory,
    /// The internal document.
    internal_doc: InternalDoc,
    /// The largest global sequence number we've assigned.
    current_seq: GlobalSeq,
    /// The length of the document. This is used to check that after
    /// applying every op, the length of the document and the length
    /// of the InternalDoc matches.
    mock_doc_len: u64,
}

impl ServerEngine {
    /// Return global ops after `seq`.
    pub fn global_ops_after(&self, seq: GlobalSeq) -> Vec<FatOp> {
        self.gh.ops_after(seq)
    }

    /// Return the current global sequence number.
    pub fn current_seq(&self) -> GlobalSeq {
        self.current_seq
    }
}

#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum EngineError {
    #[error("An op that we expect to exist isn't there: {0:?}")]
    OpMissing(FatOp),
    #[error("We expect to find {0:?}, but instead find {1:?}")]
    OpMismatch(FatOp, FatOp),
    #[error("We expect to see global sequence {0:?} in op {1:?}")]
    SeqMismatch(GlobalSeq, FatOp),
    #[error("We expect to see site sequence number {0:?} in op {1:?}")]
    SiteSeqMismatch(LocalSeq, EditorOp),
    #[error("Op {0:?} should have a global seq number, but doesn't")]
    SeqMissing(FatOp),
    #[error("Undo error: {0}")]
    UndoError(String),
}

type EngineResult<T> = Result<T, EngineError>;

// *** ClientEngine DO

impl ClientEngine {
    /// `site` is the site id of local site, `base_seq` is the
    /// starting global seq number, `init_len` is the length of the
    /// starting document.
    pub fn new(site: SiteId, base_seq: GlobalSeq, init_len: u64) -> ClientEngine {
        ClientEngine {
            gh: GlobalHistory::new(),
            internal_doc: InternalDoc::new(init_len),
            site,
            current_seq: base_seq,
            current_site_seq: 0,
            last_site_seq_sent_out: 0,
        }
    }

    /// Return whether the previous ops we sent to the server have
    /// been acked.
    fn prev_op_acked(&self) -> bool {
        if let Some(op) = self.gh.local.first() {
            if op.site_seq == self.last_site_seq_sent_out + 1 {
                return true;
            }
        }
        return false;
    }

    /// Return pending local ops.
    fn package_local_ops(&self) -> Option<ContextOps> {
        let ops: Vec<FatOp> = self.gh.local.clone();
        if ops.len() > 0 {
            Some(ContextOps {
                context: self.current_seq,
                ops,
            })
        } else {
            None
        }
    }

    /// Return packaged local ops if it's appropriate time to send
    /// them out, return None if there's no pending local ops or it's
    /// not time. (Client can only send out new local ops when
    /// previous in-flight local ops are acked by the server.) The
    /// returned ops must be sent to server for engine to work right.
    pub fn maybe_package_local_ops(&mut self) -> Option<ContextOps> {
        if self.prev_op_acked() {
            if let Some(context_ops) = self.package_local_ops() {
                let op = context_ops.ops.last().unwrap();
                self.last_site_seq_sent_out = op.site_seq;
                Some(context_ops)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Convert `op` from an internal op (with internal doc positions)
    /// to an editor op. Also apply the `op` to the internal doc.
    pub fn convert_internal_op_and_apply(&mut self, op: Op) -> EditorLeanOp {
        let converted_op = self.internal_doc.convert_internal_op_and_apply(op.clone());
        EditorLeanOp {
            op: converted_op,
            site_id: op.site(),
        }
    }

    /// Convert `ops` from an internal op to an editor op, but don't
    /// apply `ops` to the internal doc.
    pub fn convert_internal_ops_dont_apply(&mut self, ops: Vec<FatOp>) -> Vec<EditInstruction> {
        // We achieve "don't apply" by applying each op, and then
        // applying their inverse. Because for a series of ops, we
        // have to apply the first op in order to transform the later
        // op.
        let mut converted_ops = vec![];
        for op in ops.iter() {
            let converted_op = self
                .internal_doc
                .convert_internal_op_and_apply(op.op.clone());
            converted_ops.push(converted_op);
        }

        // If the user press "undo" where there's no more ops to undo,
        // we return an empty list.
        if ops.len() > 0 {
            // Now we need to reverse the affect of applying those
            // ops. Suppose ops = [1 2 3 4], now mini_history is [1 2
            // 3 4], we start from inverting 4 and applying I(4). Now
            // mini_history is [1 2 3 4 I(4)], then we inverse 3,
            // transform it against 3 4 I(4), now mini_history is [1 2
            // 3 4 I(4) I(3)], and so on.
            let mut mini_history = ops.clone();
            for idx in (mini_history.len() - 1)..0 {
                let mut op = mini_history[idx].clone();
                op.inverse();
                op.batch_transform(&mini_history[idx + 1..]);
                self.internal_doc
                    .convert_internal_op_and_apply(op.op.clone());
                mini_history.push(op);
            }
        }
        converted_ops
    }

    /// Process local op, possibly transform it and add it to history.
    pub fn process_local_op(
        &mut self,
        editor_op: EditorFatOp,
        doc: DocId,
        site_seq: LocalSeq,
    ) -> EngineResult<()> {
        log::debug!(
            "process_local_op({:?}) current_site_seq: {}",
            &editor_op,
            self.current_site_seq,
        );

        let op = editor_op.op;
        let group_seq = editor_op.group_seq;
        let kind = op.kind();

        if site_seq != self.current_site_seq + 1 {
            return Err(EngineError::SiteSeqMismatch(self.current_site_seq + 1, op));
        }
        self.current_site_seq = site_seq;

        let ops = match &op {
            EditorOp::Ins(_, _) => {
                vec![self.internal_doc.convert_editor_op_and_apply(op, self.site)]
            }
            EditorOp::Del(_, _) => {
                vec![self.internal_doc.convert_editor_op_and_apply(op, self.site)]
            }
            EditorOp::Undo => self
                .generate_undo_op_1(false)
                .into_iter()
                .map(|op| {
                    let _ = self.convert_internal_op_and_apply(op.op.clone());
                    op.op
                })
                .collect(),
            EditorOp::Redo => self
                .generate_undo_op_1(true)
                .into_iter()
                .map(|op| {
                    let _ = self.convert_internal_op_and_apply(op.op.clone());
                    op.op
                })
                .collect(),
        };

        for op in ops {
            let inferred_seq = (self.gh.global.len() + self.gh.local.len() + 1) as GlobalSeq;
            let op_kind = self.gh.process_opkind(kind, inferred_seq)?;

            self.gh.local.push(crate::op::FatOp {
                seq: None,
                doc,
                op,
                site_seq,
                kind: op_kind,
                group_seq,
            });
        }
        Ok(())
    }

    /// Process remote op, transform it and add it to history. Return
    /// the ops for the editor to apply, if any.
    pub fn process_remote_op(
        &mut self,
        mut op: FatOp,
    ) -> EngineResult<Option<(EditorLeanOp, GlobalSeq)>> {
        log::debug!(
            "process_remote_op({:?}) current_seq: {}",
            &op,
            self.current_seq
        );

        let seq = op.seq.ok_or_else(|| EngineError::SeqMissing(op.clone()))?;
        if seq != self.current_seq + 1 {
            return Err(EngineError::SeqMismatch(self.current_seq + 1, op.clone()));
        }

        if op.site() == self.site {
            // Move the op from the local part to the global part.
            if self.gh.local.len() == 0 {
                return Err(EngineError::OpMissing(op.clone()));
            }
            let local_op = self.gh.local.remove(0);
            if local_op.site() != op.site() || local_op.site_seq != op.site_seq {
                return Err(EngineError::OpMismatch(op.clone(), local_op.clone()));
            }
            self.current_seq = seq;
            self.gh.global.push(op);
            Ok(None)
        } else {
            // We received an op generated at another site, transform
            // it against local history and local history against it,
            // add it to history, convert it to editor op, and return
            // it.
            let new_local_ops = op.symmetric_transform(&self.gh.local[..]);
            self.current_seq = seq;
            self.gh.local = new_local_ops;
            self.gh.global.push(op.clone());

            // Update undo delta in local history.
            let mut offset = 0;
            for op in &mut self.gh.local {
                let kind = op.kind;
                if let OpKind::Undo(delta) = kind {
                    if offset < delta {
                        op.kind = OpKind::Undo(delta + 1);
                    }
                }
                offset += 1;
            }

            // Update inferred global seq.
            let prev_current_seq = self.current_seq - 1;
            for idx in (0..self.gh.undo_queue.len()).rev() {
                let seq = &mut self.gh.undo_queue[idx];
                if *seq > prev_current_seq {
                    // If the sequence is inferred (the refereed op is
                    // in the local history), the inferred sequence
                    // increases by 1.
                    *seq += 1;
                } else {
                    break;
                }
            }

            let seq = op.seq.unwrap();
            let op = self.convert_internal_op_and_apply(op.op);

            Ok(Some((op, seq)))
        }
    }
}

// *** ClientEngine UNDO

impl ClientEngine {
    /// Generate undo or redo ops from the current undo tip. Although
    /// this function returns [FatOp]s, most of the field of the
    /// [FatOp] (seq, site_seq, etc) are dummy values, only `site` is
    /// accurate and useful.
    fn generate_undo_op_1(&mut self, redo: bool) -> Vec<FatOp> {
        let mut idxidx;
        if redo {
            if self.gh.undo_tip.is_none() {
                return vec![];
            }
            idxidx = self.gh.undo_tip.unwrap();
        } else {
            idxidx = self
                .gh
                .undo_tip
                .or_else(|| Some(self.gh.undo_queue.len()))
                .unwrap();
            if idxidx == 0 {
                return vec![];
            }
        }

        let mut prev_group_seq = None;
        let mut ops = vec![];
        let condition = |idxidx| {
            if redo {
                idxidx < self.gh.undo_queue.len()
            } else {
                idxidx > 0
            }
        };

        // Undo/redo ops in the group.
        while condition(idxidx) {
            if !redo {
                idxidx -= 1;
            }

            let mut op = self.gh.nth_in_undo_queue(idxidx).clone();

            if let Some(p_g_seq) = prev_group_seq {
                if p_g_seq != op.group_seq {
                    break;
                }
            }
            prev_group_seq = Some(op.group_seq);

            op.inverse();
            let seq = self.gh.undo_queue[idxidx];
            let mut transform_base = self.gh.all_ops_after(seq);
            transform_base.extend_from_slice(&ops[..]);
            op.batch_transform(&transform_base);
            ops.push(op);

            if redo {
                idxidx += 1;
            }
        }
        ops
    }

    /// Generate an undo op from the current undo tip. Return an empty
    /// vector if there's no more op to undo.
    pub fn generate_undo_op(&mut self) -> Vec<EditInstruction> {
        let ops = self.generate_undo_op_1(false);
        self.convert_internal_ops_dont_apply(ops)
    }

    /// Generate a redo op from the current undo tip. Return en empty
    /// vector if there's no more ops to redo.
    pub fn generate_redo_op(&mut self) -> Vec<EditInstruction> {
        let ops = self.generate_undo_op_1(true);
        self.convert_internal_ops_dont_apply(ops)
    }

    /// Print debugging history.
    pub fn print_history(&self, debug: bool) -> String {
        if debug {
            self.gh.print_history_debug()
        } else {
            self.gh.print_history()
        }
    }
}

// *** ServerEngine

impl ServerEngine {
    /// `init_len` is the length of the starting document.
    pub fn new(init_len: u64) -> ServerEngine {
        ServerEngine {
            gh: GlobalHistory::new(),
            internal_doc: InternalDoc::new(init_len),
            current_seq: 0,
            mock_doc_len: init_len,
        }
    }

    /// Convert `op` from an internal op (with internal doc positions)
    /// to an editor op. Also apply the `op` to the internal doc.
    pub fn convert_internal_op_and_apply(&mut self, op: FatOp) -> CollabResult<EditInstruction> {
        let converted_op = self
            .internal_doc
            .convert_internal_op_and_apply(op.op.clone());

        let mut new_mock_doc_len = self.mock_doc_len;
        match &converted_op {
            EditInstruction::Ins(edits) => {
                for edit in edits {
                    if edit.0 > self.mock_doc_len as u64 {
                        return Err(CollabError::DocFatal(format!(
                            "Op out-of-range, editor_doc_len: {}, op: {:?}",
                            self.mock_doc_len, &converted_op
                        )));
                    }
                    new_mock_doc_len += edit.1.chars().count() as u64;
                }
            }
            EditInstruction::Del(edits) => {
                for edit in edits {
                    if edit.0 + edit.1.chars().count() as u64 > self.mock_doc_len as u64 {
                        return Err(CollabError::DocFatal(format!(
                            "Op out-of-range, editor_doc_len: {}, op: {:?}",
                            self.mock_doc_len, &converted_op
                        )));
                    }
                    new_mock_doc_len -= edit.1.chars().count() as u64;
                }
            }
        }
        self.mock_doc_len = new_mock_doc_len;

        // FIXME: Remove this potentially expensive test once we are
        // confident there's no position-bugs anymore.
        let internal_editor_len = self.internal_doc.editor_len();
        if internal_editor_len != self.mock_doc_len {
            return Err(CollabError::DocFatal(format!(
                "Editor doc & internal doc length mismatch, editor: {}, internal: {}",
                self.mock_doc_len, internal_editor_len
            )));
        }
        Ok(converted_op)
    }

    /// Process `op` from a client, return the transformed `ops`.
    pub fn process_ops(
        &mut self,
        mut ops: Vec<FatOp>,
        context: GlobalSeq,
    ) -> EngineResult<Vec<FatOp>> {
        let l1 = self.gh.ops_after(context);
        // Transform ops against L1, then we can append ops to global
        // history.
        (_, ops) = quatradic_transform(l1, ops);

        // Assign sequence number for each op in ops.
        for op in &mut ops {
            op.seq = Some(self.current_seq + 1);
            self.current_seq += 1;
        }
        self.gh.global.extend_from_slice(&ops[..]);
        Ok(ops)
    }
}

// *** Test

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_editor_op(doc: &mut String, op: &EditorOp) {
        match op {
            EditorOp::Ins(pos, text) => {
                doc.insert_str(*pos as usize, text);
            }
            EditorOp::Del(pos, text) => {
                doc.replace_range(*pos as usize..(*pos as usize) + text.chars().count(), "");
            }
            _ => panic!(),
        }
    }

    fn apply(doc: &mut String, op: &EditInstruction) {
        match op {
            EditInstruction::Ins(edits) => {
                for (pos, str) in edits.iter().rev() {
                    doc.insert_str(*pos as usize, &str);
                }
            }
            EditInstruction::Del(edits) => {
                for (pos, str) in edits.iter().rev() {
                    doc.replace_range((*pos as usize)..(*pos as usize + str.chars().count()), "");
                }
            }
        }
    }

    fn make_editor_fatop(op: EditorOp, group_seq: GroupSeq) -> EditorFatOp {
        EditorFatOp { op, group_seq }
    }

    // **** Puzzles

    /// deOPT puzzle, taken from II.c in "A Time Interval Based
    // Consistency Control Algorithm for Interactive Groupware
    // Applications".
    #[test]
    fn deopt_puzzle() {
        let doc_id = 1;
        let mut doc_a = "abcd".to_string();
        let mut doc_b = "abcd".to_string();

        let site_a = 1;
        let site_b = 2;
        let mut client_a = ClientEngine::new(site_a.clone(), 0, 4);
        let mut client_b = ClientEngine::new(site_b.clone(), 0, 4);

        let mut server = ServerEngine::new(0);

        let editor_op_a1 = EditorOp::Del(0, "a".to_string());
        let editor_op_a2 = EditorOp::Ins(2, "x".to_string());
        let editor_op_b1 = EditorOp::Del(2, "c".to_string());
        let op_a1 = make_editor_fatop(editor_op_a1.clone(), 1);
        let op_a2 = make_editor_fatop(editor_op_a2.clone(), 2);
        let op_b1 = make_editor_fatop(editor_op_b1.clone(), 1);

        // Local edits.
        apply_editor_op(&mut doc_a, &editor_op_a1);
        client_a.process_local_op(op_a1.clone(), doc_id, 1).unwrap();
        assert_eq!(doc_a, "bcd");
        assert!(vec_eq(
            &client_a.internal_doc.ranges,
            &vec![Range::Dead(1), Range::Live(3)]
        ));

        apply_editor_op(&mut doc_a, &editor_op_a2);
        client_a.process_local_op(op_a2.clone(), doc_id, 2).unwrap();
        assert_eq!(doc_a, "bcxd");
        assert!(vec_eq(
            &client_a.internal_doc.ranges,
            &vec![Range::Dead(1), Range::Live(4)]
        ));

        apply_editor_op(&mut doc_b, &editor_op_b1);
        client_b.process_local_op(op_b1.clone(), doc_id, 1).unwrap();
        assert_eq!(doc_b, "abd");
        assert!(vec_eq(
            &client_b.internal_doc.ranges,
            &vec![Range::Live(2), Range::Dead(1), Range::Live(1)]
        ));

        // Server processing.
        let ops_from_a = client_a.maybe_package_local_ops().unwrap();
        let ops_from_b = client_b.maybe_package_local_ops().unwrap();
        let a_ops = server
            .process_ops(ops_from_a.ops, ops_from_a.context)
            .unwrap();
        let b_ops = server
            .process_ops(ops_from_b.ops, ops_from_b.context)
            .unwrap();

        // Client A processing.
        let a1_at_a = client_a.process_remote_op(a_ops[0].clone()).unwrap();
        assert_eq!(a1_at_a, None);
        let a2_at_a = client_a.process_remote_op(a_ops[1].clone()).unwrap();
        assert_eq!(a2_at_a, None);
        let (b1_at_a, _) = client_a
            .process_remote_op(b_ops[0].clone())
            .unwrap()
            .unwrap();

        apply(&mut doc_a, &b1_at_a.op);
        assert_eq!(doc_a, "bxd");

        // Client B processing.
        let (a1_at_b, _) = client_b
            .process_remote_op(a_ops[0].clone())
            .unwrap()
            .unwrap();
        let (a2_at_b, _) = client_b
            .process_remote_op(a_ops[1].clone())
            .unwrap()
            .unwrap();
        let b1_at_b = client_b.process_remote_op(b_ops[0].clone()).unwrap();
        assert_eq!(b1_at_b, None);

        apply(&mut doc_b, &a1_at_b.op);
        assert_eq!(doc_b, "bd");

        apply(&mut doc_b, &a2_at_b.op);
        assert_eq!(doc_b, "bxd");
    }

    // False-tie puzzle, taken from II.D in "A Time Interval Based
    // Consistency Control Algorithm for Interactive Groupware
    // Applications".
    #[test]
    fn false_tie_puzzle() {
        let doc_id = 1;

        let mut doc_a = "abc".to_string();
        let mut doc_b = "abc".to_string();
        let mut doc_c = "abc".to_string();

        let site_a = 1;
        let site_b = 2;
        let site_c = 3;
        let mut client_a = ClientEngine::new(site_a.clone(), 0, 3);
        let mut client_b = ClientEngine::new(site_b.clone(), 0, 3);
        let mut client_c = ClientEngine::new(site_c.clone(), 0, 3);

        let mut server = ServerEngine::new(0);

        let editor_op_a1 = EditorOp::Ins(2, "1".to_string());
        let editor_op_b1 = EditorOp::Ins(1, "2".to_string());
        let editor_op_c1 = EditorOp::Del(1, "b".to_string());
        let op_a1 = make_editor_fatop(editor_op_a1.clone(), 1);
        let op_b1 = make_editor_fatop(editor_op_b1.clone(), 1);
        let op_c1 = make_editor_fatop(editor_op_c1.clone(), 1);

        // Local edits.
        apply_editor_op(&mut doc_a, &editor_op_a1);
        client_a.process_local_op(op_a1.clone(), doc_id, 1).unwrap();
        assert_eq!(doc_a, "ab1c");

        apply_editor_op(&mut doc_b, &editor_op_b1);
        client_b.process_local_op(op_b1.clone(), doc_id, 1).unwrap();
        assert_eq!(doc_b, "a2bc");

        apply_editor_op(&mut doc_c, &editor_op_c1);
        client_c.process_local_op(op_c1.clone(), doc_id, 1).unwrap();
        assert_eq!(doc_c, "ac");

        // Server processing.
        let ops_from_a = client_a.maybe_package_local_ops().unwrap();
        let ops_from_b = client_b.maybe_package_local_ops().unwrap();
        let ops_from_c = client_c.maybe_package_local_ops().unwrap();

        let b_ops = server
            .process_ops(ops_from_b.ops, ops_from_b.context)
            .unwrap();
        let c_ops = server
            .process_ops(ops_from_c.ops, ops_from_c.context)
            .unwrap();
        let a_ops = server
            .process_ops(ops_from_a.ops, ops_from_a.context)
            .unwrap();

        // Client A processing.
        let (b1_at_a, _) = client_a
            .process_remote_op(b_ops[0].clone())
            .unwrap()
            .unwrap();
        let (c1_at_a, _) = client_a
            .process_remote_op(c_ops[0].clone())
            .unwrap()
            .unwrap();
        let a1_at_a = client_a.process_remote_op(a_ops[0].clone()).unwrap();
        assert_eq!(a1_at_a, None);

        apply(&mut doc_a, &b1_at_a.op);
        assert_eq!(doc_a, "a2b1c");
        apply(&mut doc_a, &c1_at_a.op);
        assert_eq!(doc_a, "a21c");

        // Client B processing.
        let b1_at_b = client_b.process_remote_op(b_ops[0].clone()).unwrap();
        let (c1_at_b, _) = client_b
            .process_remote_op(c_ops[0].clone())
            .unwrap()
            .unwrap();
        let (a1_at_b, _) = client_b
            .process_remote_op(a_ops[0].clone())
            .unwrap()
            .unwrap();
        assert_eq!(b1_at_b, None);

        apply(&mut doc_b, &c1_at_b.op);
        assert_eq!(doc_b, "a2c");
        apply(&mut doc_b, &a1_at_b.op);
        assert_eq!(doc_b, "a21c");

        // Client C processing.
        let (b1_at_c, _) = client_c
            .process_remote_op(b_ops[0].clone())
            .unwrap()
            .unwrap();
        let c1_at_c = client_c.process_remote_op(c_ops[0].clone()).unwrap();
        let (a1_at_c, _) = client_c
            .process_remote_op(a_ops[0].clone())
            .unwrap()
            .unwrap();
        assert_eq!(c1_at_c, None);

        apply(&mut doc_c, &b1_at_c.op);
        assert_eq!(doc_c, "a2c");
        apply(&mut doc_c, &a1_at_c.op);
        assert_eq!(doc_c, "a21c");
    }

    /// This is what happens when you insert "{" in Emacs.
    #[test]
    fn undo() {
        let site_id = 1;
        let doc_id = 1;
        let mut client_engine = ClientEngine::new(site_id, 1, 0);
        // op1: original edit.
        let op1 = make_editor_fatop(EditorOp::Ins(0, "{".to_string()), 1);
        // Auto-insert parenthesis. I don't know why it deletes the
        // inserted bracket first, but that's what it does.
        let op2 = make_editor_fatop(EditorOp::Del(0, "{".to_string()), 1);
        let op3 = make_editor_fatop(EditorOp::Ins(0, "{".to_string()), 1);
        let op4 = make_editor_fatop(EditorOp::Ins(1, "}".to_string()), 1);
        // All four ops have the same group, which is what we want.

        client_engine.process_local_op(op1, doc_id, 1).unwrap();
        client_engine.process_local_op(op2, doc_id, 2).unwrap();
        client_engine.process_local_op(op3, doc_id, 3).unwrap();
        client_engine.process_local_op(op4, doc_id, 4).unwrap();

        let undo_ops = client_engine.generate_undo_op();

        println!("undo_ops: {:?}", undo_ops);

        assert!(undo_ops[0] == EditInstruction::Del(vec![(1, "}".to_string())]));
        assert!(undo_ops[1] == EditInstruction::Del(vec![(0, "{".to_string())]));
        assert!(undo_ops[2] == EditInstruction::Ins(vec![(0, "{".to_string())]));
        assert!(undo_ops[3] == EditInstruction::Del(vec![(0, "{".to_string())]));
    }

    #[test]
    fn test_convert_pos() {
        let mut doc = InternalDoc::new(0);
        doc.ranges = vec![Range::Live(10), Range::Dead(10), Range::Live(10)];
        let mut cursor = doc.get_cursor(&0);

        // Cursor go forward.
        assert!(doc.convert_pos(10, &mut cursor, true) == 10);
        // The cursor must point to a range after calling
        // `convert_pos`.
        assert!(cursor.editor_pos == 0 && cursor.internal_pos == 0 && cursor.range_idx == Some(0));

        assert!(doc.convert_pos(11, &mut cursor, true) == 21);
        assert!(
            cursor.editor_pos == 10 && cursor.internal_pos == 20 && cursor.range_idx == Some(2)
        );

        assert!(doc.convert_pos(20, &mut cursor, true) == 30);
        assert!(
            cursor.editor_pos == 10 && cursor.internal_pos == 20 && cursor.range_idx == Some(2)
        );

        // Cursor go back.
        assert!(doc.convert_pos(15, &mut cursor, true) == 25);
        assert!(
            cursor.editor_pos == 10 && cursor.internal_pos == 20 && cursor.range_idx == Some(2)
        );

        assert!(doc.convert_pos(9, &mut cursor, true) == 9);
        assert!(cursor.editor_pos == 0 && cursor.internal_pos == 0 && cursor.range_idx == Some(0));

        // Internal pos to editor pos.
        assert!(doc.convert_pos(1, &mut cursor, false) == 1);
        assert!(cursor.editor_pos == 0 && cursor.internal_pos == 0 && cursor.range_idx == Some(0));

        assert!(doc.convert_pos(15, &mut cursor, false) == 10);
        assert!(
            cursor.editor_pos == 10 && cursor.internal_pos == 10 && cursor.range_idx == Some(1)
        );

        assert!(doc.convert_pos(20, &mut cursor, false) == 10);
        assert!(
            cursor.editor_pos == 10 && cursor.internal_pos == 10 && cursor.range_idx == Some(1)
        );

        assert!(doc.convert_pos(21, &mut cursor, false) == 11);
        assert!(
            cursor.editor_pos == 10 && cursor.internal_pos == 20 && cursor.range_idx == Some(2)
        );
    }

    fn vec_eq<T: Eq>(vec1: &Vec<T>, vec2: &Vec<T>) -> bool {
        vec1.len() == vec2.len() && vec1.iter().zip(vec2).filter(|&(a, b)| a != b).count() == 0
    }

    fn make_str(n: usize) -> String {
        (0..n).map(|_| "x").collect()
    }

    #[test]
    fn test_apply_ins() {
        let mut doc = InternalDoc::new(0);
        let mut cursor = doc.get_cursor(&0);

        doc.cursors.insert(
            1,
            Cursor {
                editor_pos: 0,
                internal_pos: 0,
                range_idx: None,
            },
        );

        // (a)
        doc.convert_pos(0, &mut cursor, true);
        doc.apply_ins(0, 10, &mut cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Live(10)]));
        assert!(cursor.internal_pos == 0);
        assert!(cursor.editor_pos == 0);
        // [Live(10)]
        let cursor1 = doc.cursors.get(&1).unwrap();
        assert!(cursor1.internal_pos == 0);
        assert!(cursor1.editor_pos == 0);
        assert!(cursor1.range_idx == None);

        // (b)
        doc.convert_pos(0, &mut cursor, true);
        doc.apply_ins(0, 10, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Live(20)]));
        assert!(cursor.internal_pos == 0);
        assert!(cursor.editor_pos == 0);
        assert!(cursor.range_idx == Some(0));
        // [Live(20)]

        doc.cursors.insert(
            2,
            Cursor {
                editor_pos: 20,
                internal_pos: 20,
                range_idx: Some(1),
            },
        );

        // (c)
        doc.ranges.push(Range::Dead(10));
        // [Live(20) Dead(10)]
        cursor.internal_pos = 20;
        cursor.editor_pos = 20;
        cursor.range_idx = Some(1);
        doc.apply_ins(20, 10, &mut cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Live(30), Range::Dead(10)]));
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(cursor.internal_pos == 30);
        assert!(cursor.editor_pos == 30);
        assert!(cursor.range_idx == Some(1));
        // [Live(30) Dead(10)]

        let cursor2 = doc.cursors.get(&2).unwrap();
        assert!(cursor2.internal_pos == 30);
        assert!(cursor2.editor_pos == 30);
        assert!(cursor2.range_idx == Some(1));

        // (d)
        doc.ranges.remove(0);
        // [Dead(10)]
        cursor.internal_pos = 0;
        cursor.editor_pos = 0;
        cursor.range_idx = Some(0);
        doc.apply_ins(0, 10, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Live(10), Range::Dead(10)]));
        assert!(cursor.internal_pos == 10);
        assert!(cursor.editor_pos == 10);
        assert!(cursor.range_idx == Some(1));
        // [Live(10) Dead(10)];

        // (f)
        cursor.internal_pos = 10;
        cursor.editor_pos = 10;
        cursor.range_idx = Some(1);
        doc.apply_ins(20, 10, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(
            &doc.ranges,
            &vec![Range::Live(10), Range::Dead(10), Range::Live(10)]
        ));
        // [Live(10) Dead(10) Live(10)]

        // (e)
        doc.ranges.push(Range::Dead(10));
        // [Live(10) Dead(10) Live(10) Dead(10)]
        doc.apply_ins(20, 10, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(
            &doc.ranges,
            &vec![
                Range::Live(10),
                Range::Dead(10),
                Range::Live(20),
                Range::Dead(10),
            ]
        ));
        // [Live(10) Dead(10) Live(20) Dead(10)]

        doc.cursors.insert(
            3,
            Cursor {
                editor_pos: 30,
                internal_pos: 40,
                range_idx: Some(3),
            },
        );

        // (g)
        doc.apply_ins(16, 10, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(
            &doc.ranges,
            &vec![
                Range::Live(10),
                Range::Dead(6),
                Range::Live(10),
                Range::Dead(4),
                Range::Live(20),
                Range::Dead(10),
            ]
        ));
        assert!(cursor.internal_pos == 16);
        assert!(cursor.editor_pos == 10);
        assert!(cursor.range_idx == Some(2));

        let cursor3 = doc.cursors.get(&3).unwrap();
        assert!(cursor3.internal_pos == 50);
        assert!(cursor3.editor_pos == 40);
        assert!(cursor3.range_idx == Some(5));
    }

    #[test]
    fn test_apply_mark_dead() {
        let mut doc = InternalDoc::new(0);
        let mut cursor = doc.get_cursor(&0);
        cursor.range_idx = Some(0);

        // Case 1.
        // 0           10    15    20          30          40    41
        // [   Live   ][Dead][Live][   Dead   ][   Live   ][dead]
        //       [              Marked               ]
        //       6                                    36
        doc.ranges = vec![
            Range::Live(10),
            Range::Dead(5),
            Range::Live(5),
            Range::Dead(10),
            Range::Live(10),
            Range::Dead(1),
        ];
        doc.cursors.insert(
            1,
            Cursor {
                editor_pos: 10,
                internal_pos: 10,
                range_idx: Some(1),
            },
        );
        doc.cursors.insert(
            2,
            Cursor {
                editor_pos: 15,
                internal_pos: 30,
                range_idx: Some(4),
            },
        );
        doc.cursors.insert(
            3,
            Cursor {
                editor_pos: 25,
                internal_pos: 40,
                range_idx: Some(5),
            },
        );

        let flipped_ranges = doc.apply_mark_dead(6, 30, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(
            &doc.ranges,
            &vec![
                Range::Live(6),
                Range::Dead(30),
                Range::Live(4),
                Range::Dead(1)
            ]
        ));
        assert!(vec_eq(&flipped_ranges, &vec![(6, 4), (15, 5), (30, 6)]));
        let cursor1 = doc.get_cursor(&1);
        assert!(cursor1.internal_pos == cursor.internal_pos);
        assert!(cursor1.editor_pos == cursor.editor_pos);
        assert!(cursor1.range_idx == cursor.range_idx);
        let cursor2 = doc.get_cursor(&2);
        assert!(cursor2.internal_pos == cursor.internal_pos);
        assert!(cursor2.editor_pos == cursor.editor_pos);
        assert!(cursor2.range_idx == cursor.range_idx);
        let cursor3 = doc.get_cursor(&3);
        assert!(cursor3.internal_pos == 40);
        assert!(cursor3.editor_pos == 10);
        assert!(cursor3.range_idx == Some(3));

        // Case 2.
        // 0           10    15          25    30          40
        // [   Dead   ][Live][   Dead   ][Live][   Dead   ]
        //       [              Marked               ]
        //       6                                    36
        doc.ranges = vec![
            Range::Dead(10),
            Range::Live(5),
            Range::Dead(10),
            Range::Live(5),
            Range::Dead(10),
        ];

        let flipped_ranges = doc.apply_mark_dead(6, 30, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Dead(40)]));
        assert!(vec_eq(&flipped_ranges, &vec![(10, 5), (25, 5)]));

        // Case 3.
        // [   Dead   ]
        // [  Marked  ]
        doc.ranges = vec![Range::Dead(10)];
        cursor.internal_pos = 0;
        cursor.editor_pos = 0;
        cursor.range_idx = Some(0);

        let flipped_ranges = doc.apply_mark_dead(0, 10, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Dead(10)]));
        assert!(vec_eq(&flipped_ranges, &vec![]));

        // Case 4.
        // [   Live   ]
        // [  Marked  ]
        doc.ranges = vec![Range::Live(10)];
        cursor.internal_pos = 0;
        cursor.editor_pos = 0;
        cursor.range_idx = Some(0);

        let flipped_ranges = doc.apply_mark_dead(0, 10, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Dead(10)]));
        assert!(vec_eq(&flipped_ranges, &vec![(0, 10)]));

        // Case 5.
        // [      Live      ]
        //    [  Marked  ]
        doc.ranges = vec![Range::Live(20)];
        cursor.internal_pos = 0;
        cursor.editor_pos = 0;
        cursor.range_idx = Some(0);

        let flipped_ranges = doc.apply_mark_dead(6, 10, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(
            &doc.ranges,
            &vec![Range::Live(6), Range::Dead(10), Range::Live(4)]
        ));
        assert!(vec_eq(&flipped_ranges, &vec![(6, 10)]));
        assert!(cursor.internal_pos == 0);
        assert!(cursor.editor_pos == 0);
        assert!(cursor.range_idx == Some(0));

        // Case 6.
        // [ Dead ][ Live ][ Dead ]
        //         [ Dead ]
        doc.ranges = vec![Range::Dead(10), Range::Live(10), Range::Dead(10)];
        cursor.internal_pos = 10;
        cursor.editor_pos = 0;
        cursor.range_idx = Some(1);
        let flipped_ranges = doc.apply_mark_dead(10, 10, &mut cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Dead(30)]));
        assert!(vec_eq(&flipped_ranges, &vec![(10, 10)]));
        assert!(cursor.internal_pos == 0);
        assert!(cursor.editor_pos == 0);
        assert!(cursor.range_idx == Some(0));
    }

    #[test]
    fn test_apply_mark_live() {
        let mut doc = InternalDoc::new(0);
        let mut cursor = doc.get_cursor(&0);
        cursor.range_idx = Some(0);

        // Case 1.
        // 0           10    15    20          30          40    41
        // [   Live   ][Dead][Live][   Dead   ][   Live   ][Dead]
        //       [              Marked               ]
        //       6                                    36
        doc.ranges = vec![
            Range::Live(10),
            Range::Dead(5),
            Range::Live(5),
            Range::Dead(10),
            Range::Live(10),
            Range::Dead(1),
        ];
        doc.cursors.insert(
            1,
            Cursor {
                editor_pos: 10,
                internal_pos: 10,
                range_idx: Some(1),
            },
        );
        doc.cursors.insert(
            2,
            Cursor {
                editor_pos: 15,
                internal_pos: 30,
                range_idx: Some(4),
            },
        );
        doc.cursors.insert(
            3,
            Cursor {
                editor_pos: 25,
                internal_pos: 40,
                range_idx: Some(5),
            },
        );

        doc.apply_mark_live(6, 30, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Live(40), Range::Dead(1)]));

        let cursor1 = doc.get_cursor(&1);
        assert!(cursor1.internal_pos == cursor.internal_pos);
        assert!(cursor1.editor_pos == cursor.editor_pos);
        assert!(cursor1.range_idx == cursor.range_idx);
        let cursor2 = doc.get_cursor(&2);
        assert!(cursor2.internal_pos == cursor.internal_pos);
        assert!(cursor2.editor_pos == cursor.editor_pos);
        assert!(cursor2.range_idx == cursor.range_idx);
        let cursor3 = doc.get_cursor(&3);
        assert!(cursor3.internal_pos == 40);
        assert!(cursor3.editor_pos == 40);
        assert!(cursor3.range_idx == Some(1));

        // Case 2.
        // 0           10    15          25    30          40
        // [   Dead   ][Live][   Dead   ][Live][   Dead   ]
        //       [              Marked               ]
        //       6                                    36
        doc.ranges = vec![
            Range::Dead(10),
            Range::Live(5),
            Range::Dead(10),
            Range::Live(5),
            Range::Dead(10),
        ];

        doc.apply_mark_live(6, 30, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(
            &doc.ranges,
            &vec![Range::Dead(6), Range::Live(30), Range::Dead(4)]
        ));

        // Case 3.
        // [   Dead   ]
        // [  Marked  ]
        doc.ranges = vec![Range::Dead(10)];
        cursor.internal_pos = 0;
        cursor.editor_pos = 0;
        cursor.range_idx = Some(0);

        doc.apply_mark_live(0, 10, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Live(10)]));

        // Case 4.
        // [   Live   ]
        // [  Marked  ]
        doc.ranges = vec![Range::Live(10)];
        cursor.internal_pos = 0;
        cursor.editor_pos = 0;
        cursor.range_idx = Some(0);

        doc.apply_mark_live(0, 10, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Live(10)]));

        // Case 5.
        // [      Dead      ]
        //    [  Marked  ]
        doc.ranges = vec![Range::Dead(20)];
        cursor.internal_pos = 0;
        cursor.editor_pos = 0;
        cursor.range_idx = Some(0);

        doc.apply_mark_live(6, 10, &mut cursor);
        println!("Ranges {:#?}", doc.ranges);
        println!("{:#?}", cursor);
        assert!(vec_eq(
            &doc.ranges,
            &vec![Range::Dead(6), Range::Live(10), Range::Dead(4)]
        ));
        assert!(cursor.internal_pos == 0);
        assert!(cursor.editor_pos == 0);
        assert!(cursor.range_idx == Some(0));

        // Case 6.
        // [ Live ][ Dead ][ Live ]
        //         [ Live ]
        doc.ranges = vec![Range::Live(10), Range::Dead(10), Range::Live(10)];
        cursor.internal_pos = 10;
        cursor.editor_pos = 10;
        cursor.range_idx = Some(1);
        doc.apply_mark_live(10, 10, &mut cursor);
        assert!(vec_eq(&doc.ranges, &vec![Range::Live(30)]));
        assert!(cursor.internal_pos == 0);
        assert!(cursor.editor_pos == 0);
        assert!(cursor.range_idx == Some(0));
    }

    #[test]
    fn test_mark_to_ins_or_del() {
        let mut doc = InternalDoc::new(0);
        let mut cursor = doc.get_cursor(&0);
        cursor.range_idx = Some(0);

        // Case 1.
        // [   Live   ][Dead][Live][   Dead   ][   Live   ]
        //      [              Live               ]
        doc.ranges = vec![
            Range::Live(10),
            Range::Dead(5),
            Range::Live(5),
            Range::Dead(10),
            Range::Live(10),
        ];

        let mut ranges = vec![];
        doc.mark_to_ins_or_del(6, make_str(30), &cursor, &mut ranges, true);
        println!("Ranges {:#?}", ranges);
        assert!(vec_eq(
            &ranges,
            &vec![(10, make_str(5)), (15, make_str(10))]
        ));

        // Case 2.
        // [   Live   ][Dead][Live][   Dead   ][   Live   ]
        //      [              Dead               ]
        doc.ranges = vec![
            Range::Live(10),
            Range::Dead(5),
            Range::Live(5),
            Range::Dead(10),
            Range::Live(10),
        ];

        let mut ranges = vec![];
        doc.mark_to_ins_or_del(6, make_str(30), &cursor, &mut ranges, false);
        println!("Ranges {:#?}", ranges);
        assert!(vec_eq(
            &ranges,
            &vec![(6, make_str(4)), (10, make_str(5)), (15, make_str(6))]
        ));

        // Case 3.
        // [   Dead   ]
        // [   Live   ]
        doc.ranges = vec![Range::Dead(10)];
        let mut ranges = vec![];
        doc.mark_to_ins_or_del(0, make_str(10), &cursor, &mut ranges, true);
        assert!(vec_eq(&ranges, &vec![(0, make_str(10))]));

        // Case 4.
        // [   Live   ]
        // [   Dead   ]
        doc.ranges = vec![Range::Live(10)];
        let mut ranges = vec![];
        doc.mark_to_ins_or_del(0, make_str(10), &cursor, &mut ranges, false);
        assert!(vec_eq(&ranges, &vec![(0, make_str(10))]));
    }
}
