//! This module defines OT operations and various types. [Op] is the
//! one we actually use, [SimpleOp] was used for prototyping.

use prost::encoding::bool;
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
/// be a small, personal tool, so 10k should be enough TM.
pub type DocId = u32;
/// SiteId is a monotonically increasing integer.
pub type SiteId = u32;
/// Group sequence number. Consecutive ops with the same group seq
/// number are undone together.
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
}

// *** Op and tranform functions

/// An string-wise operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Insertion.
    //   pos  content
    Ins((u64, String)),
    /// Deletion.
    //       pos  content
    Del(Vec<(u64, String)>),
}

/// Simple char-based op used for testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimpleOp {
    Ins(u64, char),
    Del(u64, Option<char>),
}

impl Operation for SimpleOp {
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

fn transform_dd(
    op: &(u64, String),
    base: &(u64, String),
    op_site: &SiteId,
    base_site: &SiteId,
) -> (u64, String) {
    let pos1 = op.0;
    let pos2 = base.0;
    let content1 = &op.1;
    let content2 = &base.1;
    let end1 = pos1 + content1.len() as u64;
    let end2 = pos2 + content2.len() as u64;

    if pos_less_than(end1, pos2, op_site, base_site) {
        // op completely in front of base.
        // | op |   | base |
        (pos1, content1.clone())
    } else if pos_less_than(pos1, pos2, op_site, base_site)
        && pos_less_than(end1, end2, op_site, base_site)
    {
        // op partially in front of base.
        // | op |
        //   | base |
        (pos1, content1[0..((pos2 - pos1) as usize)].to_string())
    } else if pos_less_than(pos1, pos2, op_site, base_site)
        && pos_less_than(end2, end1, base_site, op_site)
    {
        // op contains base.
        // |   op   |
        //   |base|
        let mut new_content = content1[..((pos2 - pos1) as usize)].to_string();
        new_content.push_str(&content1[((end2 - pos1) as usize)..]);
        (pos1, new_content)
    } else if pos_less_than(end2, pos1, base_site, op_site) {
        // op completely after base.
        // | base |   | op |
        (pos1 - (end2 - pos2), content1.clone())
    } else if pos_less_than(end2, end1, base_site, op_site)
        && pos_less_than(pos2, pos1, base_site, op_site)
    {
        // op partially after base.
        // | base |
        //   |  op  |
        (pos2, content1[((end2 - pos1) as usize)..].to_string())
    } else {
        // op completely inside base.
        // | base |
        //  | op |
        (pos2, "".to_string())
    }
}

impl Operation for Op {
    fn transform(&self, base: &Op, self_site: &SiteId, base_site: &SiteId) -> Op {
        match (self, base) {
            (Op::Ins(op), Op::Ins(base)) => Op::Ins(transform_ii(op, base, self_site, base_site)),
            (Op::Ins(op), Op::Del(bases)) => {
                let mut new_op = op.clone();
                for base in bases {
                    new_op = transform_id(&new_op, base, self_site, base_site);
                }
                Op::Ins(new_op)
            }
            (Op::Del(ops), Op::Ins(base)) => {
                let mut new_ops = vec![];
                for op in ops {
                    new_ops.extend(transform_di(op, base, self_site, base_site));
                }
                Op::Del(new_ops)
            }
            (Op::Del(ops), Op::Del(bases)) => {
                let mut new_ops = ops.clone();
                for op in &mut new_ops {
                    for base in bases {
                        *op = transform_dd(&op, base, self_site, base_site);
                    }
                }
                Op::Del(new_ops)
            }
        }
    }
}

impl Op {
    /// Create the inverse of self. If the op is a deletion with more
    /// than one range, only the first range is reversed and the rest
    /// are ignored. (As for now, we only need to reverse original
    /// ops, and original delete op only have one range.)
    pub fn inverse(&self) -> Op {
        match self {
            Op::Ins((pos, str)) => Op::Del(vec![(*pos, str.clone())]),
            Op::Del(ops) => {
                assert!(ops.len() == 1);
                Op::Ins((ops[0].0, ops[0].1.clone()))
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
    /// Site-local sequence number. TODO: is this necessary?
    pub site_seq: LocalSeq,
    /// The kind of this op.
    pub kind: OpKind,
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
}

// *** Functions for Vec<FatOp>

/// Iterate over `ops`, and finds out all the ops to skip during
/// transformation. An op is skipped if there is an undo op that
/// undoes it. Returns a bitmap, if bitmap[idx] = true, skip ops[idx]
/// during transformation.
pub fn find_ops_to_skip<O>(ops: &[FatOp<O>]) -> Vec<bool> {
    if ops.len() == 0 {
        return vec![];
    }
    let mut bitmap = vec![false; ops.len()];
    for idx in ops.len() - 1..0 {
        if let OpKind::Undo(delta) = ops[idx].kind {
            if bitmap[idx] {
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
            Op::Del(vec![(2, "y".to_string())]),
            Op::Ins((1, "x".to_string())),
        );

        println!("Ins Del, op > base.");
        test(
            Op::Ins((2, "x".to_string())),
            Op::Del(vec![(0, "y".to_string())]),
            Op::Ins((1, "x".to_string())),
        );

        println!("Ins Del, op inside base.");
        test(
            Op::Ins((1, "x".to_string())),
            Op::Del(vec![(0, "yyy".to_string())]),
            Op::Ins((0, "x".to_string())),
        );
    }

    // I'm too tired to refactor the tests below.
    #[test]
    fn test_transform_di() {
        println!("Del Ins, op < base.");
        let mut op = make_fatop(Op::Del(vec![(1, "xxx".to_string())]), 1);
        let base = make_fatop(Op::Ins((4, "y".to_string())), 2);
        let result_op = make_fatop(Op::Del(vec![(1, "xxx".to_string())]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Ins, base inside op.");
        let mut op = make_fatop(Op::Del(vec![(1, "xxx".to_string())]), 1);
        let base = make_fatop(Op::Ins((2, "y".to_string())), 2);
        let result_op = make_fatop(
            Op::Del(vec![(1, "x".to_string()), (3, "xx".to_string())]),
            1,
        );
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Ins, op > base");
        let mut op = make_fatop(Op::Del(vec![(1, "xxx".to_string())]), 1);
        let base = make_fatop(Op::Ins((0, "y".to_string())), 2);
        let result_op = make_fatop(Op::Del(vec![(2, "xxx".to_string())]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);
    }

    #[test]
    fn test_transform_dd() {
        println!("Del Del, op completely before base.");
        let mut op = make_fatop(Op::Del(vec![(1, "xxx".to_string())]), 1);
        let base = make_fatop(Op::Del(vec![(4, "y".to_string())]), 2);
        let result_op = make_fatop(Op::Del(vec![(1, "xxx".to_string())]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Del, op partially before base.");
        let mut op = make_fatop(Op::Del(vec![(1, "oxx".to_string())]), 1);
        let base = make_fatop(Op::Del(vec![(2, "xxy".to_string())]), 2);
        let result_op = make_fatop(Op::Del(vec![(1, "o".to_string())]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Del, op completely inside base.");
        let mut op = make_fatop(Op::Del(vec![(1, "xx".to_string())]), 1);
        let base = make_fatop(Op::Del(vec![(0, "ooxxy".to_string())]), 2);
        let result_op = make_fatop(Op::Del(vec![(0, "".to_string())]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Del, op completely after base.");
        let mut op = make_fatop(Op::Del(vec![(4, "xx".to_string())]), 1);
        let base = make_fatop(Op::Del(vec![(1, "yy".to_string())]), 2);
        let result_op = make_fatop(Op::Del(vec![(2, "xx".to_string())]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Del, op partially after base.");
        let mut op = make_fatop(Op::Del(vec![(2, "xxyy".to_string())]), 1);
        let base = make_fatop(Op::Del(vec![(1, "oxx".to_string())]), 2);
        let result_op = make_fatop(Op::Del(vec![(1, "yy".to_string())]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);

        println!("Del Del, op completely covers base.");
        let mut op = make_fatop(Op::Del(vec![(1, "ooxxyy".to_string())]), 1);
        let base = make_fatop(Op::Del(vec![(3, "xx".to_string())]), 2);
        let result_op = make_fatop(Op::Del(vec![(1, "ooyy".to_string())]), 1);
        op.transform(&base);
        assert_eq!(op, result_op);
    }

    #[test]
    fn test_batch_transform_with_skip_1() {
        // A do and an undo. Start with "XX" in the doc.
        let ops = vec![
            make_fatop(Op::Del(vec![(0, "X".to_string())]), 1),
            make_fatop(Op::Ins((0, "A".to_string())), 1),
            make_undo_fatop(Op::Ins((1, "X".to_string())), 1, 2),
        ];
        let mut op = make_fatop(Op::Ins((1, "C".to_string())), 2);
        let result_op = make_fatop(Op::Ins((2, "C".to_string())), 2);
        op.batch_transform(&ops[..]);
        assert_eq!(op, result_op);
    }

    #[test]
    fn test_batch_transform_with_skip_2() {
        // A do and an undo and a redo. Start with "XX" in the doc.
        let ops = vec![
            make_fatop(Op::Del(vec![(0, "X".to_string())]), 1),
            make_fatop(Op::Ins((0, "A".to_string())), 1),
            make_undo_fatop(Op::Ins((1, "X".to_string())), 1, 2),
            make_fatop(Op::Ins((1, "B".to_string())), 1),
            make_undo_fatop(Op::Del(vec![(2, "X".to_string())]), 1, 2),
        ];
        let mut op = make_fatop(Op::Ins((1, "C".to_string())), 2);
        let result_op = make_fatop(Op::Ins((2, "C".to_string())), 2);
        op.batch_transform(&ops[..]);
        assert_eq!(op, result_op);
    }
}
