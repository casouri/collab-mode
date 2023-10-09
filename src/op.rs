//! This module defines OT operations and various types. [Op] is the
//! one we actually use, [SimpleOp] was used for prototyping.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// *** Types

/// Local sequence number, unique on the same site, starts from 1.
pub type LocalSeq = u32;
/// Global sequence number, globally unique, starts from 1.
pub type GlobalSeq = u32;
/// A DocId is a randomly generated integer. I'd really like to use
/// u64, but JSON can't encode u64. u32 allows about 10k documents on
/// a single server with reasonable collision. I intend collab-mode to
/// be a small, personal tool, so 10k should be enough(TM).
pub type DocId = u32;
/// SiteId is a monotonically increasing integer.
pub type SiteId = u32;
/// Group sequence number. Consecutive ops with the same group seq
/// number are undone together. Group seqs don't have to be
/// continuous.
pub type GroupSeq = u32;

#[derive(Debug, Eq, PartialEq, Clone, Copy, Deserialize, Serialize)]
pub enum OpKind {
    /// Original op.
    Original,
    /// An undo op that undoes the ops this many counts before the
    /// referrer op in history.
    Undo(usize),
}

// *** Trait

pub trait Operation: std::fmt::Debug + Clone + PartialEq + Eq {
    fn transform(&self, base: &Self, self_site: &SiteId, base_site: &SiteId) -> Self;
    fn inverse(&mut self);
}

// *** Op and tranform functions

/// An string-wise operation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Insertion.
    //   pos  content
    Ins((u64, String)),
    /// Deletion.
    //       pos  content   Recover list
    Del(Vec<(u64, String)>, Vec<(u64, String)>),
}

/// Simple char-based op used for testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimpleOp {
    Ins(u64, char),
    Del(u64, Option<char>),
}

impl Operation for SimpleOp {
    fn inverse(&mut self) {
        todo!()
    }

    fn transform(&self, base: &SimpleOp, self_site: &SiteId, base_site: &SiteId) -> SimpleOp {
        match (self, base) {
            (SimpleOp::Ins(pos1, char1), SimpleOp::Ins(pos2, _)) => {
                if pos_less_than(*pos1, *pos2, self_site, base_site) {
                    SimpleOp::Ins(*pos1, *char1)
                } else {
                    SimpleOp::Ins(pos1 + 1, *char1)
                }
            }
            (SimpleOp::Ins(pos1, char1), SimpleOp::Del(pos2, Some(_))) => {
                if pos_less_than(*pos1, *pos2, self_site, base_site) {
                    SimpleOp::Ins(*pos1, *char1)
                } else {
                    SimpleOp::Ins(pos1 - 1, *char1)
                }
            }
            (SimpleOp::Ins(pos1, char1), SimpleOp::Del(_, None)) => SimpleOp::Ins(*pos1, *char1),
            (SimpleOp::Del(pos1, char1), SimpleOp::Ins(pos2, _)) => {
                if pos_less_than(*pos1, *pos2, self_site, base_site) {
                    SimpleOp::Del(*pos1, *char1)
                } else {
                    SimpleOp::Del(pos1 + 1, *char1)
                }
            }
            (SimpleOp::Del(pos1, char1), SimpleOp::Del(pos2, Some(_))) => {
                if *pos1 < *pos2 {
                    SimpleOp::Del(*pos1, *char1)
                } else if *pos1 == *pos2 {
                    SimpleOp::Del(*pos1, None)
                } else {
                    SimpleOp::Del(pos1 - 1, *char1)
                }
            }
            (SimpleOp::Del(pos1, char1), SimpleOp::Del(_, None)) => SimpleOp::Del(*pos1, *char1),
        }
    }
}

fn pos_less_than(pos1: u64, pos2: u64, site1: &SiteId, site2: &SiteId) -> bool {
    pos1 < pos2 || (pos1 == pos2 && site1 < site2)
}

fn transform_ii(
    op: &(u64, String),
    base: &(u64, String),
    op_site: &SiteId,
    base_site: &SiteId,
) -> (u64, String) {
    let pos1 = op.0;
    let pos2 = base.0;
    let content1 = &op.1;
    let content2 = &base.1;

    if pos_less_than(pos1, pos2, op_site, base_site) {
        (pos1, content1.clone())
    } else {
        // If both pos and site are equal, the op is pushed forward
        // (pos + 1), and base stays the same.
        let new_pos = pos1 + content2.len() as u64;
        (new_pos, content1.clone())
    }
}

fn transform_id(
    op: &(u64, String),
    base: &(u64, String),
    op_site: &SiteId,
    base_site: &SiteId,
) -> (u64, String) {
    let pos1 = op.0;
    let pos2 = base.0;
    let content1 = &op.1;
    let content2 = &base.1;
    let end2 = pos2 + content2.len() as u64;

    if pos_less_than(pos1, pos2, op_site, base_site) {
        // Op completely in front of base.
        (pos1, content1.clone())
    } else if pos_less_than(pos1, end2, op_site, base_site) {
        // Op inside base.
        (pos2, content1.clone())
    } else {
        // Op completely after base.
        let new_pos = pos1 - (end2 - pos2);
        (new_pos, content1.clone())
    }
}

fn transform_di(
    op: &(u64, String),
    base: &(u64, String),
    op_site: &SiteId,
    base_site: &SiteId,
) -> Vec<(u64, String)> {
    let pos1 = op.0;
    let pos2 = base.0;
    let content1 = &op.1;
    let content2 = &base.1;
    let end1 = pos1 + content1.len() as u64;
    let end2 = pos2 + content2.len() as u64;

    if pos_less_than(end1, pos2, op_site, base_site) {
        // op completely before base.
        vec![(pos1, content1.clone())]
    } else if pos_less_than(pos2, pos1, base_site, op_site) {
        // op completely after base.
        let new_pos = pos1 + content2.len() as u64;
        vec![(new_pos, content1.clone())]
    } else {
        // base inside op.
        let del_before_ins = (pos1, content1[..((pos2 - pos1) as usize)].to_string());
        let del_after_ins = (end2, content1[((pos2 - pos1) as usize)..].to_string());
        let mut res = vec![];
        if del_before_ins.1.len() > 0 {
            res.push(del_before_ins)
        }
        if del_after_ins.1.len() > 0 {
            res.push(del_after_ins)
        }
        res
    }
}

/// Merge `segment` into `ops`. (They shouldn't overlap.) `ops` can
/// either be the segments in the del op, or the recover list.
fn merge_in(mut segment: (u64, String), ops: &mut Vec<(u64, String)>) {
    for idx in 0..ops.len() {
        let op_start = ops[idx].0;
        let op_end = op_start + ops[idx].1.len() as u64;
        let segment_start = segment.0;
        let segment_end = segment_start + segment.1.len() as u64;
        // Verify no overlap.
        assert!(op_end <= segment_start || segment_end <= op_start);
        if segment_start == op_end {
            ops[idx].1.push_str(&segment.1);
            return;
        }
        if segment_end == op_start {
            segment.1.push_str(&ops[idx].1);
            ops[idx].1 = segment.1;
            return;
        }
    }
    ops.push(segment);
}

fn transform_di_with_recover(
    ops: &[(u64, String)],
    base: &(u64, String),
    op_site: &SiteId,
    base_site: &SiteId,
    recover_list: &[(u64, String)],
) -> (Vec<(u64, String)>, Vec<(u64, String)>) {
    let mut new_ops = vec![];
    for op in ops {
        if op.1.len() > 0 {
            new_ops.extend(transform_di(op, base, op_site, base_site));
        }
    }

    // Anything that we can recover?
    let base_start = base.0;
    let base_end = base_start + base.1.len() as u64;
    let mut new_recover_list = vec![];
    for recover_op in recover_list {
        let recover_op_start = recover_op.0;
        let recover_op_end = recover_op_start + recover_op.1.len() as u64;
        //      |  recover_op  |
        // |  base  |
        if recover_op_start < base_end && recover_op_end > base_start {
            let mid = (base_end - recover_op_start) as usize;
            let recover_op_merge = (recover_op_start, recover_op.1[..mid].to_string());
            let recover_op_stay = (base_end, recover_op.1[mid..].to_string());
            merge_in(recover_op_merge, &mut new_ops);
            if recover_op_stay.1.len() > 0 {
                new_recover_list.push(recover_op_stay);
            }
        }
        // |  recover_op  |
        //           |  base  |
        else if recover_op_end > base_start && recover_op_start < base_end {
            let mid = (base_start - recover_op_start) as usize;
            let recover_op_stay = (recover_op_start, recover_op.1[..mid].to_string());
            let recover_op_merge = (base_start, recover_op.1[mid..].to_string());
            merge_in(recover_op_merge, &mut new_ops);
            if recover_op_stay.1.len() > 0 {
                new_recover_list.push(recover_op_stay);
            }
        } else {
            new_recover_list.push(recover_op.clone());
        }
    }
    (new_ops, new_recover_list)
}

/// Returns (transformed_op, recover_op)
fn transform_dd_with_recover(
    op: &(u64, String),
    base: &(u64, String),
    op_site: &SiteId,
    base_site: &SiteId,
) -> ((u64, String), Option<(u64, String)>) {
    let pos1 = op.0;
    let pos2 = base.0;
    let content1 = &op.1;
    let content2 = &base.1;
    let end1 = pos1 + content1.len() as u64;
    let end2 = pos2 + content2.len() as u64;

    if pos_less_than(end1, pos2, op_site, base_site) {
        // op completely in front of base.
        // | op |   | base |
        ((pos1, content1.clone()), None)
    } else if pos_less_than(pos1, pos2, op_site, base_site)
        && pos_less_than(end1, end2, op_site, base_site)
    {
        // op partially in front of base.
        // | op |
        //   | base |
        let mid = (pos2 - pos1) as usize;
        let recover_op = if end1 == pos2 {
            None
        } else {
            Some((pos2, content1[mid..].to_string()))
        };
        ((pos1, content1[..mid].to_string()), recover_op)
    } else if pos_less_than(pos1, pos2, op_site, base_site)
        && pos_less_than(end2, end1, base_site, op_site)
    {
        // op contains base.
        // |   op   |
        //   |base|
        let mut new_content = content1[..((pos2 - pos1) as usize)].to_string();
        new_content.push_str(&content1[((end2 - pos1) as usize)..]);
        ((pos1, new_content), Some((pos2, content2.to_string())))
    } else if pos_less_than(end2, pos1, base_site, op_site) {
        // op completely after base.
        // | base |   | op |
        ((pos1 - (end2 - pos2), content1.clone()), None)
    } else if pos_less_than(end2, end1, base_site, op_site)
        && pos_less_than(pos2, pos1, base_site, op_site)
    {
        // op partially after base.
        // | base |
        //   |  op  |
        let mid = (end2 - pos1) as usize;
        let recover_op = if end2 == pos1 {
            None
        } else {
            Some((pos1, content1[..mid].to_string()))
        };
        ((pos2, content1[mid..].to_string()), recover_op)
    } else {
        // op completely inside base.
        // | base |
        //  | op |
        ((pos2, "".to_string()), Some((pos1, content1.to_string())))
    }
}

impl Operation for Op {
    /// Create the inverse of self. If the op is a deletion with more
    /// than one range, only the first range is reversed and the rest
    /// are ignored. (As for now, we only need to reverse original
    /// ops, and original delete op only have one range.)
    fn inverse(&mut self) {
        match self {
            Op::Ins((pos, str)) => *self = Op::Del(vec![(*pos, str.clone())], vec![]),
            Op::Del(ops, _) => {
                assert!(ops.len() == 1);
                *self = Op::Ins((ops[0].0, ops[0].1.clone()));
            }
        }
    }

    fn transform(&self, base: &Op, self_site: &SiteId, base_site: &SiteId) -> Op {
        match (self, base) {
            (Op::Ins(op), Op::Ins(base)) => Op::Ins(transform_ii(op, base, self_site, base_site)),
            (Op::Ins(op), Op::Del(bases, _)) => {
                let mut new_op = op.clone();
                for base in bases {
                    new_op = transform_id(&new_op, base, self_site, base_site);
                }
                Op::Ins(new_op)
            }
            (Op::Del(ops, recover_list), Op::Ins(base)) => {
                let (ops, recover_list) =
                    transform_di_with_recover(ops, base, self_site, base_site, &recover_list);
                Op::Del(ops, recover_list)
            }
            (Op::Del(ops, recover_list), Op::Del(bases, _)) => {
                let mut new_ops = ops.clone();
                let mut new_recover_list = recover_list.clone();
                for op in &mut new_ops {
                    for base in bases {
                        let (new_op, recover_op) =
                            transform_dd_with_recover(&op, base, self_site, base_site);
                        *op = new_op;
                        if let Some(rec_op) = recover_op {
                            merge_in(rec_op, &mut new_recover_list);
                        }
                    }
                }
                Op::Del(new_ops, new_recover_list)
            }
        }
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::Ins((pos, content)) => {
                let mut content = content.to_string();
                content = content.replace("\t", "⭾");
                content = content.replace("\n", "⮐");
                write!(f, "ins({pos}, {content})")
            }
            Op::Del(ops, _) => {
                let mut out = String::new();
                for op in ops {
                    out += format!("del({}, {}) ", op.0, op.1).as_str();
                }
                write!(f, "{}", out)
            }
        }
    }
}

impl std::fmt::Debug for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::Ins((pos, content)) => {
                let mut content = content.to_string();
                write!(f, "ins({pos}, {:?})", content)
            }
            Op::Del(ops, recover_list) => {
                let mut out = String::new();
                for op in ops {
                    out += format!("del({}, {:?}) ", op.0, op.1).as_str();
                }
                for op in recover_list {
                    out += format!("rec({}, {:?}) ", op.0, op.1).as_str();
                }
                write!(f, "{}", out)
            }
        }
    }
}

// *** FatOp

/// Op with meta info.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
pub struct FatOp<O> {
    /// Global sequence number. If None, the op is a locally generated
    /// op that hasn't been transmitted to the server yet. If not
    /// None, it's a globally recognized op that has been
    /// sequentialized by the server.
    pub seq: Option<GlobalSeq>,
    /// Document uuid.
    pub doc: DocId,
    /// The operation.
    pub op: O,
    /// Site uuid. The site that generated this op.
    pub site: SiteId,
    /// Site-local sequence number.
    pub site_seq: LocalSeq,
    /// The kind of this op.
    pub kind: OpKind,
    /// The group sequence.
    pub group_seq: GroupSeq,
}

impl<O: Operation> FatOp<O> {
    /// Transform `self` against another op `base`. The two op must
    /// have the same context.
    pub fn transform(&mut self, base: &FatOp<O>) {
        self.op = self.op.transform(&base.op, &self.site, &base.site);
    }

    /// Transform `self` against every op in `ops` sequentially.
    pub fn batch_transform(&mut self, ops: &[FatOp<O>]) {
        let skip_map = find_ops_to_skip(ops);
        self.batch_transform_1(ops, &skip_map[..])
    }

    /// Transform `self` against every op in `ops` sequentially.
    /// `skip_map` is a bitmap that tells whether to skip an op in
    /// `ops`. If skip_map[idx] is true, ops[idx] is considered as
    /// identity.
    fn batch_transform_1(&mut self, ops: &[FatOp<O>], skip_map: &[bool]) {
        let mut idx = 0;
        for op in ops {
            if !skip_map[idx] {
                self.transform(op);
            }
            idx += 0;
        }
    }

    /// Transform `self` against every op in `ops` sequentially. In
    /// the meantime, transform every op in `ops` against `self`, and
    /// return the new `ops`.
    pub fn symmetric_transform(&mut self, ops: &[FatOp<O>]) -> Vec<FatOp<O>> {
        let skip_map = find_ops_to_skip(ops);
        self.symmetric_transform_1(ops, &skip_map[..])
    }

    /// Transform `self` against every op in `ops` sequentially. In
    /// the meantime, transform every op in `ops` against `self`, and
    /// return the new `ops`. `skip_map` is a bitmap that tells
    /// whether to skip an op in `ops`. If skip_map[idx] is true,
    /// ops[idx] is considered as identity. Regardless of skip_map,
    /// every op in `ops` are transformed against `op`.
    fn symmetric_transform_1(&mut self, ops: &[FatOp<O>], skip_map: &[bool]) -> Vec<FatOp<O>> {
        let mut new_ops = vec![];
        let mut idx = 0;
        for op in ops {
            let mut new_op = op.clone();
            new_op.transform(self);
            if !skip_map[idx] {
                self.transform(op);
            }
            new_ops.push(new_op);
            idx += 1;
        }
        new_ops
    }

    /// Inverse the op.
    pub fn inverse(&mut self) {
        self.op.inverse();
    }
}

// *** Functions for Vec<FatOp>

/// Iterate over `ops`, and finds out all the ops to skip during
/// transformation. An op is skipped if there is an undo op that
/// undoes it. Returns a bitmap, if bitmap[i] = true, skip ops[i]
/// during transformation.
pub fn find_ops_to_skip<O>(ops: &[FatOp<O>]) -> Vec<bool> {
    if ops.len() == 0 {
        return vec![];
    }
    let mut bitmap = vec![false; ops.len()];
    for idx in (0..ops.len()).rev() {
        if let OpKind::Undo(delta) = ops[idx].kind {
            if !bitmap[idx] && idx >= delta {
                // It's possible for delta to be greater than idx, in
                // that case the inverse is in `ops` but the original
                // isn't, and we don't need to skip the inverse.
                bitmap[idx] = true;
                bitmap[idx - delta] = true;
            }
        }
    }
    bitmap
}

/// Transform ops1 against ops2, and transform ops2 against ops1. Return the transformed ops1 and ops2.
pub fn quatradic_transform<O: Operation>(
    mut ops1: Vec<FatOp<O>>,
    mut ops2: Vec<FatOp<O>>,
) -> (Vec<FatOp<O>>, Vec<FatOp<O>>) {
    let skip1 = find_ops_to_skip(&ops1[..]);
    let skip2 = find_ops_to_skip(&ops2[..]);

    let mut idx2 = 0;
    for op2 in &mut ops2 {
        if !skip2[idx2] {
            // Every op in ops1 are transformed against op2, in the
            // meantime, op2 are transformed against all non-identity
            // ops in ops1.
            ops1 = op2.symmetric_transform_1(&ops1[..], &skip1[..]);
        } else {
            // If op2 is identify, only transform op2 against ops1,
            // but don't transform ops1 against op2.
            op2.batch_transform_1(&ops1[..], &skip1[..]);
        }
        idx2 += 1;
    }
    (ops1, ops2)
}

// *** Tests
#[cfg(test)]
mod tests {
    use super::*;

    fn make_fatop<O: Operation>(op: O, site: SiteId) -> FatOp<O> {
        FatOp {
            seq: None,
            site,
            site_seq: 1, // Dummy value.
            doc: 0,
            op,
            kind: OpKind::Original,
            group_seq: 1, // Dummy value.
        }
    }

    fn make_undo_fatop<O: Operation>(op: O, site: SiteId, undo_delta: usize) -> FatOp<O> {
        FatOp {
            seq: None,
            site,
            site_seq: 1, // Dummy value.
            doc: 0,
            op,
            kind: OpKind::Undo(undo_delta),
            group_seq: 1, // Dummy value.
        }
    }

    fn test<O: Operation>(op: O, base: O, result: O) {
        let mut op = make_fatop(op, 1);
        let base = make_fatop(base, 2);
        let result_op = make_fatop(result, 1);
        op.transform(&base);
        assert_eq!(op, result_op);
    }

    #[test]
    fn test_simple_transform() {
        println!("Ins Ins, op < base.");
        test(
            SimpleOp::Ins(1, 'x'),
            SimpleOp::Ins(2, 'y'),
            SimpleOp::Ins(1, 'x'),
        );

        println!("Ins Ins, op > base.");
        test(
            SimpleOp::Ins(2, 'x'),
            SimpleOp::Ins(1, 'y'),
            SimpleOp::Ins(3, 'x'),
        );

        println!("Ins Del, op < base.");
        test(
            SimpleOp::Ins(1, 'x'),
            SimpleOp::Del(2, Some('y')),
            SimpleOp::Ins(1, 'x'),
        );

        println!("Ins Del, op > base.");
        test(
            SimpleOp::Ins(2, 'x'),
            SimpleOp::Del(1, Some('y')),
            SimpleOp::Ins(1, 'x'),
        );

        println!("Del Ins, op < base.");
        test(
            SimpleOp::Del(1, Some('x')),
            SimpleOp::Ins(2, 'y'),
            SimpleOp::Del(1, Some('x')),
        );

        println!("Del Ins, op > base.");
        test(
            SimpleOp::Del(2, Some('x')),
            SimpleOp::Ins(1, 'y'),
            SimpleOp::Del(3, Some('x')),
        );

        println!("Del Del, op < base.");
        test(
            SimpleOp::Del(1, Some('x')),
            SimpleOp::Del(2, Some('y')),
            SimpleOp::Del(1, Some('x')),
        );

        println!("Del Del, op = base.");
        test(
            SimpleOp::Del(1, Some('x')),
            SimpleOp::Del(1, Some('x')),
            SimpleOp::Del(1, None),
        );

        println!("Del Del, op > base.");
        test(
            SimpleOp::Del(2, Some('x')),
            SimpleOp::Del(1, Some('y')),
            SimpleOp::Del(1, Some('x')),
        );
    }

    #[test]
    fn test_transform_ii() {
        println!("Ins Ins, op < base.");
        test(
            Op::Ins((1, "x".to_string())),
            Op::Ins((2, "y".to_string())),
            Op::Ins((1, "x".to_string())),
        );

        println!("Ins Ins, op > base.");
        test(
            Op::Ins((1, "x".to_string())),
            Op::Ins((0, "y".to_string())),
            Op::Ins((2, "x".to_string())),
        );
    }

    #[test]
    fn test_transform_id() {
        println!("Ins Del, op < base.");
        test(
            Op::Ins((1, "x".to_string())),
            Op::Del(vec![(2, "y".to_string())], vec![]),
            Op::Ins((1, "x".to_string())),
        );

        println!("Ins Del, op > base.");
        test(
            Op::Ins((2, "x".to_string())),
            Op::Del(vec![(0, "y".to_string())], vec![]),
            Op::Ins((1, "x".to_string())),
        );

        println!("Ins Del, op inside base.");
        test(
            Op::Ins((1, "x".to_string())),
            Op::Del(vec![(0, "yyy".to_string())], vec![]),
            Op::Ins((0, "x".to_string())),
        );
    }

    // I'm too tired to refactor the tests below.
    #[test]
    fn test_transform_di() {
        println!("Del Ins, op < base.");
        let mut op = make_fatop(Op::Del(vec![(1, "xxx".to_string())], vec![]), 1);
        let base = make_fatop(Op::Ins((4, "y".to_string())), 2);
        let result_op = make_fatop(Op::Del(vec![(1, "xxx".to_string())], vec![]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Ins, base inside op.");
        let mut op = make_fatop(Op::Del(vec![(1, "xxx".to_string())], vec![]), 1);
        let base = make_fatop(Op::Ins((2, "y".to_string())), 2);
        let result_op = make_fatop(
            Op::Del(vec![(1, "x".to_string()), (3, "xx".to_string())], vec![]),
            1,
        );
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Ins, op > base");
        let mut op = make_fatop(Op::Del(vec![(1, "xxx".to_string())], vec![]), 1);
        let base = make_fatop(Op::Ins((0, "y".to_string())), 2);
        let result_op = make_fatop(Op::Del(vec![(2, "xxx".to_string())], vec![]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);
    }

    #[test]
    fn test_transform_dd() {
        println!("Del Del, op completely before base.");
        let mut op = make_fatop(Op::Del(vec![(1, "xxx".to_string())], vec![]), 1);
        let base = make_fatop(Op::Del(vec![(4, "y".to_string())], vec![]), 2);
        let result_op = make_fatop(Op::Del(vec![(1, "xxx".to_string())], vec![]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Del, op partially before base.");
        let mut op = make_fatop(Op::Del(vec![(1, "oxx".to_string())], vec![]), 1);
        let base = make_fatop(Op::Del(vec![(2, "xxy".to_string())], vec![]), 2);
        let result_op = make_fatop(
            Op::Del(vec![(1, "o".to_string())], vec![(2, "xx".to_string())]),
            1,
        );
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Del, op completely inside base.");
        let mut op = make_fatop(Op::Del(vec![(1, "xx".to_string())], vec![]), 1);
        let base = make_fatop(Op::Del(vec![(0, "ooxxy".to_string())], vec![]), 2);
        let result_op = make_fatop(
            Op::Del(vec![(0, "".to_string())], vec![(1, "xx".to_string())]),
            1,
        );
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Del, op completely after base.");
        let mut op = make_fatop(Op::Del(vec![(4, "xx".to_string())], vec![]), 1);
        let base = make_fatop(Op::Del(vec![(1, "yy".to_string())], vec![]), 2);
        let result_op = make_fatop(Op::Del(vec![(2, "xx".to_string())], vec![]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Del, op partially after base.");
        let mut op = make_fatop(Op::Del(vec![(2, "xxyy".to_string())], vec![]), 1);
        let base = make_fatop(Op::Del(vec![(1, "oxx".to_string())], vec![]), 2);
        let result_op = make_fatop(
            Op::Del(vec![(1, "yy".to_string())], vec![(2, "xx".to_string())]),
            1,
        );
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Del, op completely covers base.");
        let mut op = make_fatop(Op::Del(vec![(1, "ooxxyy".to_string())], vec![]), 1);
        let base = make_fatop(Op::Del(vec![(3, "xx".to_string())], vec![]), 2);
        let result_op = make_fatop(
            Op::Del(vec![(1, "ooyy".to_string())], vec![(3, "xx".to_string())]),
            1,
        );
        op.transform(&base);
        assert_eq!(op, result_op);
    }

    // TODO: Better and more tricky tests.
    #[test]
    fn test_batch_transform_with_skip_1() {
        // A do and an undo. Start with "XX" in the doc.
        let ops = vec![
            make_fatop(Op::Del(vec![(0, "X".to_string())], vec![]), 1),
            make_fatop(Op::Ins((0, "A".to_string())), 1),
            make_undo_fatop(Op::Ins((1, "X".to_string())), 1, 2),
        ];
        let mut op = make_fatop(Op::Ins((1, "C".to_string())), 2);
        let result_op = make_fatop(Op::Ins((1, "C".to_string())), 2);
        // We expect "ACX".
        op.batch_transform(&ops[..]);
        assert_eq!(op, result_op);
    }

    #[test]
    fn test_batch_transform_with_skip_2() {
        // A do and an undo and a redo. Start with "XX" in the doc.
        let ops = vec![
            make_fatop(Op::Del(vec![(0, "X".to_string())], vec![]), 1),
            make_fatop(Op::Ins((0, "A".to_string())), 1),
            make_undo_fatop(Op::Ins((1, "X".to_string())), 1, 2),
            make_fatop(Op::Ins((1, "B".to_string())), 1),
            make_undo_fatop(Op::Del(vec![(2, "X".to_string())], vec![]), 1, 2),
        ];
        let mut op = make_fatop(Op::Ins((1, "C".to_string())), 2);
        let result_op = make_fatop(Op::Ins((2, "C".to_string())), 2);
        // We expect "ABCX".
        op.batch_transform(&ops[..]);
        assert_eq!(op, result_op);
    }

    #[test]
    fn test_find_ops_to_skip_1() {
        let ops = vec![
            make_fatop(Op::Del(vec![(0, "X".to_string())], vec![]), 1),
            make_fatop(Op::Ins((0, "A".to_string())), 1),
            make_undo_fatop(Op::Ins((1, "X".to_string())), 1, 2),
            make_fatop(Op::Ins((1, "B".to_string())), 1),
            make_undo_fatop(Op::Del(vec![(2, "X".to_string())], vec![]), 1, 2),
        ];
        let bitmap = find_ops_to_skip(&ops[..]);
        assert_eq!(bitmap, vec![false, false, true, false, true]);
    }

    #[test]
    fn test_find_ops_to_skip_2() {
        let ops = vec![
            make_fatop(Op::Del(vec![(0, "X".to_string())], vec![]), 1),
            make_fatop(Op::Ins((0, "A".to_string())), 1),
            make_undo_fatop(Op::Ins((1, "X".to_string())), 1, 3),
            //                                                ^
        ];
        let bitmap = find_ops_to_skip(&ops[..]);
        assert_eq!(bitmap, vec![false, false, false]);
    }
}
