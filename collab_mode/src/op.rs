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
    fn transform(&self, base: &Self) -> Self;
    fn inverse(&mut self);
}

// *** Op and tranform functions

/// An string-wise operation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Insertion.
    //   pos  content
    Ins((u64, String), SiteId),
    /// Deletion.
    //       pos  content   live/dead
    Mark(Vec<(u64, String)>, bool, SiteId),
}

impl Op {
    /// Return the site of the op.
    pub fn site(&self) -> SiteId {
        match self {
            Self::Ins(_, site) => *site,
            Self::Mark(_, _, site) => *site,
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
        let new_pos = pos1 + content2.chars().count() as u64;
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
    let end1 = pos1 + content1.chars().count() as u64;
    let end2 = pos2 + content2.chars().count() as u64;

    if pos_less_than(end1, pos2, op_site, base_site) {
        // op completely before base.
        vec![(pos1, content1.clone())]
    } else if pos_less_than(pos2, pos1, base_site, op_site) {
        // op completely after base.
        let new_pos = pos1 + content2.chars().count() as u64;
        vec![(new_pos, content1.clone())]
    } else {
        // base inside op.
        let mid = (pos2 - pos1) as usize;
        // let del_before_ins = (pos1, content1[..((pos2 - pos1) as usize)].to_string());
        let del_before_ins = (pos1, content1.chars().take(mid).collect::<String>());
        // let del_after_ins = (end2, content1[((pos2 - pos1) as usize)..].to_string());
        let del_after_ins = (end2, content1.chars().skip(mid).collect::<String>());
        let mut res = vec![];
        if del_before_ins.1.chars().count() > 0 {
            res.push(del_before_ins)
        }
        if del_after_ins.1.chars().count() > 0 {
            res.push(del_after_ins)
        }
        res
    }
}

impl Operation for Op {
    /// Create the inverse of this op.
    fn inverse(&mut self) {
        match self {
            Op::Ins((pos, str), site) => {
                *self = Op::Mark(vec![(*pos, str.clone())], false, site.clone())
            }
            Op::Mark(ops, live, site) => *self = Op::Mark(ops.clone(), !(*live), site.clone()),
        }
    }

    fn transform(&self, base: &Op) -> Op {
        match (self, base) {
            (Op::Ins(op, self_site), Op::Ins(base, base_site)) => Op::Ins(
                transform_ii(op, base, self_site, base_site),
                self_site.clone(),
            ),
            (Op::Mark(ops, live, self_site), Op::Ins(base, base_site)) => {
                let mut new_ops = vec![];
                for op in ops.iter() {
                    new_ops.extend(transform_di(op, base, self_site, base_site));
                }
                Op::Mark(new_ops, live.clone(), self_site.clone())
            }
            _ => self.clone(),
        }
    }
}

pub fn replace_whitespace_char(string: String) -> String {
    // Other return symbols aren't well supported by terminals.
    string.replace("\n", "\\n")
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::Ins((pos, content), _) => {
                let content = replace_whitespace_char(content.to_string());
                write!(f, "ins({pos}, {content})")
            }
            Op::Mark(ops, live, _) => {
                let mut out = String::new();
                if ops.len() == 0 {
                    out += "[]";
                }
                for op in ops {
                    let content = replace_whitespace_char(op.1.clone());
                    if *live {
                        out += format!("mark_live({}, {}) ", op.0, content).as_str();
                    } else {
                        out += format!("mark_dead({}, {}) ", op.0, content).as_str();
                    }
                }
                write!(f, "{}", out)
            }
        }
    }
}

impl std::fmt::Debug for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::Ins((pos, content), _) => {
                let content = content.to_string();
                write!(f, "ins({pos}, {:?})", content)
            }
            Op::Mark(ops, live, _) => {
                let mut out = String::new();
                for op in ops {
                    let content = replace_whitespace_char(op.1.clone());
                    if *live {
                        out += format!("mark_live({}, {}) ", op.0, content).as_str();
                    } else {
                        out += format!("mark_dead({}, {}) ", op.0, content).as_str();
                    }
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
    /// The operation.
    pub op: O,
    /// Site-local sequence number.
    pub site_seq: LocalSeq,
    /// The kind of this op.
    pub kind: OpKind,
    /// The group sequence.
    pub group_seq: GroupSeq,
}

impl<T> FatOp<T> {
    /// Swap the op of this [FatOp] with `op`.
    pub fn swap<P>(self, op: P) -> FatOp<P> {
        FatOp {
            seq: self.seq,
            op,
            site_seq: self.site_seq,
            kind: self.kind,
            group_seq: self.group_seq,
        }
    }
}

impl FatOp<Op> {
    /// Return the site of the op.
    pub fn site(&self) -> SiteId {
        self.op.site()
    }
}

impl<O: Operation> FatOp<O> {
    /// Transform `self` against another op `base`. The two op must
    /// have the same context.
    pub fn transform(&mut self, base: &FatOp<O>) {
        self.op = self.op.transform(&base.op);
    }

    /// Transform `self` against every op in `ops` sequentially.
    pub fn batch_transform(&mut self, ops: &[FatOp<O>]) {
        for op in ops {
            self.transform(op);
        }
    }

    /// Transform `self` against every op in `ops` sequentially. In
    /// the meantime, transform every op in `ops` against `self`, and
    /// return the new `ops`.
    pub fn symmetric_transform(&mut self, ops: &[FatOp<O>]) -> Vec<FatOp<O>> {
        let mut new_ops = vec![];
        for op in ops {
            let mut new_op = op.clone();
            new_op.transform(self);
            self.transform(op);
            new_ops.push(new_op);
        }
        new_ops
    }

    /// Inverse the op.
    pub fn inverse(&mut self) {
        self.op.inverse();
    }
}

// *** Functions for Vec<FatOp>

/// Transform ops1 against ops2, and transform ops2 against ops1.
/// Return the transformed ops1 and ops2. Pass the shorter list as
/// `ops1` because it's copied, and `ops2` are transformed in-place.
pub fn quatradic_transform<O: Operation>(
    mut ops1: Vec<FatOp<O>>,
    mut ops2: Vec<FatOp<O>>,
) -> (Vec<FatOp<O>>, Vec<FatOp<O>>) {
    for op2 in &mut ops2 {
        ops1 = op2.symmetric_transform(&ops1[..]);
    }
    (ops1, ops2)
}

// *** Tests
#[cfg(test)]
mod tests {
    use super::*;

    fn test<O: Operation>(op: O, base: O, result: O) {
        let new_op = op.transform(&base);
        assert_eq!(new_op, result);
    }

    #[test]
    fn test_transform_ii() {
        println!("Ins Ins, op < base.");
        test(
            Op::Ins((1, "x".to_string()), 1),
            Op::Ins((2, "y".to_string()), 2),
            Op::Ins((1, "x".to_string()), 1),
        );

        println!("Ins Ins, op > base.");
        test(
            Op::Ins((1, "x".to_string()), 1),
            Op::Ins((0, "y".to_string()), 2),
            Op::Ins((2, "x".to_string()), 1),
        );
    }

    // Transform_id is now trivial.
    // #[test]
    // fn test_transform_id() {
    //     println!("Ins Del, op < base.");
    //     test(
    //         Op::Ins((1, "x".to_string())),
    //         Op::Mark(vec![(2, "y".to_string())], false),
    //         Op::Ins((1, "x".to_string())),
    //     );

    //     println!("Ins Del, op > base.");
    //     test(
    //         Op::Ins((2, "x".to_string())),
    //         Op::Del(vec![(0, "y".to_string())], false),
    //         Op::Ins((2, "x".to_string())),
    //     );

    //     println!("Ins Del, op inside base.");
    //     test(
    //         Op::Ins((1, "x".to_string())),
    //         Op::Del(vec![(0, "yyy".to_string())], false),
    //         Op::Ins((1, "x".to_string())),
    //     );
    // }

    #[test]
    fn test_transform_di() {
        println!("Del Ins, op < base.");
        test(
            Op::Mark(vec![(1, "xxx".to_string())], false, 1),
            Op::Ins((4, "y".to_string()), 2),
            Op::Mark(vec![(1, "xxx".to_string())], false, 1),
        );

        println!("Del Ins, base inside op.");
        test(
            Op::Mark(vec![(1, "xxx".to_string())], false, 1),
            Op::Ins((2, "y".to_string()), 2),
            Op::Mark(vec![(1, "x".to_string()), (3, "xx".to_string())], false, 1),
        );

        println!("Del Ins, op > base");
        test(
            Op::Mark(vec![(1, "xxx".to_string())], false, 1),
            Op::Ins((0, "y".to_string()), 2),
            Op::Mark(vec![(2, "xxx".to_string())], false, 1),
        );
    }

    // Transform_dd is now trivial.
    //
    // #[test]
    // fn test_transform_dd() {
    //     println!("Del Del, op completely before base.");
    //     let mut op = make_fatop(Op::Del(vec![(1, "xxx".to_string())], vec![]), 1);
    //     let base = make_fatop(Op::Del(vec![(4, "y".to_string())], vec![]), 2);
    //     let result_op = make_fatop(Op::Del(vec![(1, "xxx".to_string())], vec![]), 1);
    //     op.transform(&base);
    //     assert_eq!(op, result_op);

    //     println!("Del Del, op partially before base.");
    //     let mut op = make_fatop(Op::Del(vec![(1, "oxx".to_string())], vec![]), 1);
    //     let base = make_fatop(Op::Del(vec![(2, "xxy".to_string())], vec![]), 2);
    //     let result_op = make_fatop(
    //         Op::Del(vec![(1, "o".to_string())], vec![(2, "xx".to_string())]),
    //         1,
    //     );
    //     op.transform(&base);
    //     assert_eq!(op, result_op);

    //     println!("Del Del, op completely inside base.");
    //     let mut op = make_fatop(Op::Del(vec![(1, "xx".to_string())], vec![]), 1);
    //     let base = make_fatop(Op::Del(vec![(0, "ooxxy".to_string())], vec![]), 2);
    //     let result_op = make_fatop(
    //         Op::Del(vec![(0, "".to_string())], vec![(1, "xx".to_string())]),
    //         1,
    //     );
    //     op.transform(&base);
    //     assert_eq!(op, result_op);

    //     println!("Del Del, op completely after base.");
    //     let mut op = make_fatop(Op::Del(vec![(4, "xx".to_string())], vec![]), 1);
    //     let base = make_fatop(Op::Del(vec![(1, "yy".to_string())], vec![]), 2);
    //     let result_op = make_fatop(Op::Del(vec![(2, "xx".to_string())], vec![]), 1);
    //     op.transform(&base);
    //     assert_eq!(op, result_op);

    //     println!("Del Del, op partially after base.");
    //     let mut op = make_fatop(Op::Del(vec![(2, "xxyy".to_string())], vec![]), 1);
    //     let base = make_fatop(Op::Del(vec![(1, "oxx".to_string())], vec![]), 2);
    //     let result_op = make_fatop(
    //         Op::Del(vec![(1, "yy".to_string())], vec![(2, "xx".to_string())]),
    //         1,
    //     );
    //     op.transform(&base);
    //     assert_eq!(op, result_op);

    //     println!("Del Del, op completely covers base.");
    //     let mut op = make_fatop(Op::Del(vec![(1, "ooxxyy".to_string())], vec![]), 1);
    //     let base = make_fatop(Op::Del(vec![(3, "xx".to_string())], vec![]), 2);
    //     let result_op = make_fatop(
    //         Op::Del(vec![(1, "ooyy".to_string())], vec![(3, "xx".to_string())]),
    //         1,
    //     );
    //     op.transform(&base);
    //     assert_eq!(op, result_op);
    // }

    // We don't skip now.
    //
    // // TODO: Better and more tricky tests.
    // #[test]
    // fn test_batch_transform_with_skip_1() {
    //     // A do and an undo. Start with "XX" in the doc.
    //     let ops = vec![
    //         make_fatop(Op::Del(vec![(0, "X".to_string())], vec![]), 1),
    //         make_fatop(Op::Ins((0, "A".to_string())), 1),
    //         make_undo_fatop(Op::Ins((1, "X".to_string())), 1, 2),
    //     ];
    //     let mut op = make_fatop(Op::Ins((1, "C".to_string())), 2);
    //     let result_op = make_fatop(Op::Ins((1, "C".to_string())), 2);
    //     // We expect "ACX".
    //     op.batch_transform(&ops[..]);
    //     assert_eq!(op, result_op);
    // }

    // #[test]
    // fn test_batch_transform_with_skip_2() {
    //     // A do and an undo and a redo. Start with "XX" in the doc.
    //     let ops = vec![
    //         make_fatop(Op::Del(vec![(0, "X".to_string())], vec![]), 1),
    //         make_fatop(Op::Ins((0, "A".to_string())), 1),
    //         make_undo_fatop(Op::Ins((1, "X".to_string())), 1, 2),
    //         make_fatop(Op::Ins((1, "B".to_string())), 1),
    //         make_undo_fatop(Op::Del(vec![(2, "X".to_string())], vec![]), 1, 2),
    //     ];
    //     let mut op = make_fatop(Op::Ins((1, "C".to_string())), 2);
    //     let result_op = make_fatop(Op::Ins((2, "C".to_string())), 2);
    //     // We expect "ABCX".
    //     op.batch_transform(&ops[..]);
    //     assert_eq!(op, result_op);
    // }

    // #[test]
    // fn test_find_ops_to_skip_1() {
    //     let ops = vec![
    //         make_fatop(Op::Del(vec![(0, "X".to_string())], vec![]), 1),
    //         make_fatop(Op::Ins((0, "A".to_string())), 1),
    //         make_undo_fatop(Op::Ins((1, "X".to_string())), 1, 2),
    //         make_fatop(Op::Ins((1, "B".to_string())), 1),
    //         make_undo_fatop(Op::Del(vec![(2, "X".to_string())], vec![]), 1, 2),
    //     ];
    //     let bitmap = find_ops_to_skip(&ops[..]);
    //     assert_eq!(bitmap, vec![false, false, true, false, true]);
    // }

    // #[test]
    // fn test_find_ops_to_skip_2() {
    //     let ops = vec![
    //         make_fatop(Op::Del(vec![(0, "X".to_string())], vec![]), 1),
    //         make_fatop(Op::Ins((0, "A".to_string())), 1),
    //         make_undo_fatop(Op::Ins((1, "X".to_string())), 1, 3),
    //         //                                                ^
    //     ];
    //     let bitmap = find_ops_to_skip(&ops[..]);
    //     assert_eq!(bitmap, vec![false, false, false]);
    // }
}
