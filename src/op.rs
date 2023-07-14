use prost::encoding::bool;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// *** Types

/// Local sequence number, unique on the same site, starts from 1.
pub type LocalSeq = u32;
/// Global sequence number, globally unique, starts from 1.
pub type GlobalSeq = u32;
/// A DocId is a randomly generated integer.
pub type DocId = u32;
/// SiteId is a monotonically increasing integer.
pub type SiteId = u32;

#[derive(Debug, Eq, PartialEq, Clone, Copy, Deserialize, Serialize)]
pub enum OpKind {
    Original,
    Undo,
    Redo,
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
    /// Site-local sequence number.
    pub site_seq: LocalSeq,
}

impl<O: Operation> FatOp<O> {
    /// Transform `self` against another op `base`. The two op must
    /// have the same context.
    pub fn transform(&mut self, base: &FatOp<O>) {
        self.op = self.op.transform(&base.op, &self.site, &base.site);
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
}

// *** LeanOp

/// Op with less meta info, used by client history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
pub struct LeanOp<O> {
    pub op: O,
    pub site: SiteId,
    /// `orig` holds the original op that's added to the history.
    /// Later transformation (due to undo/redo) alters `op` but not
    /// `orig`.
    pub orig: O,
    /// When the op is undone, `identify` is marked true so that
    /// transformation function treats this op as identify.
    pub identity: bool,
}

impl<O: Operation> LeanOp<O> {
    pub fn new(op: &O, site: &SiteId) -> LeanOp<O> {
        LeanOp {
            op: op.clone(),
            site: site.clone(),
            orig: op.clone(),
            identity: false,
        }
    }

    /// Transform `self` against another op `base`. The two op must
    /// have the same context.
    pub fn transform(&mut self, base: &LeanOp<O>) {
        if !base.identity {
            self.op = self.op.transform(&base.op, &self.site, &base.site);
        }
    }

    /// Transform `self` against every op in `ops` sequentially. In
    /// the meantime, transform every op in `ops` against `self`, and
    /// return the new `ops`.
    pub fn symmetric_transform(&mut self, ops: &[LeanOp<O>]) -> Vec<LeanOp<O>> {
        let mut new_ops = vec![];
        for op in ops {
            let mut new_op = op.clone();
            new_op.transform(self);
            self.transform(op);
            new_ops.push(new_op);
        }
        new_ops
    }
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
        }
    }

    fn test<O: Operation>(op: O, base: O, result: O) {
        let mut op = make_fatop(op, 1);
        let base = make_fatop(base, 2);
        let result_op = make_fatop(result, 3);
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
}
