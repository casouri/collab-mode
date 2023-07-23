//! This module provides [ClientEngine] and [ServerEngine] that
//! implements the client and server part of the OT control algorithm.

use crate::{op::quatradic_transform, types::*};
use serde::{Deserialize, Serialize};
use std::result::Result;
use thiserror::Error;

// *** Data structure

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
        self.ops[0].site.clone()
    }
    /// Return the doc of the ops.
    pub fn doc(&self) -> DocId {
        self.ops[0].doc.clone()
    }
}

/// Global history buffer. The whole buffer is made of global_buffer +
/// local_buffer.
#[derive(Debug, Clone)]
struct GlobalHistory {
    /// The global ops section.
    global: Vec<FatOp>,
    /// The local ops section. The local part is made of many context
    /// op groups.
    local: Vec<ContextOps>,
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

/// Return the ops in `ops` whose global seq is greater than `seq`.
/// Note that `ops` must be made of only globals ops.
fn ops_after(ops: Vec<FatOp>, seq: GlobalSeq) -> EngineResult<Vec<FatOp>> {
    let mut idx = 0;
    for op in &ops {
        if op.seq.is_none() {
            return Err(EngineError::SeqMissing(op.clone()));
        }
        if op.seq.unwrap() <= seq {
            idx += 1;
        } else {
            break;
        }
    }
    if idx < ops.len() {
        Ok(ops[idx..].to_vec())
    } else {
        Ok(vec![])
    }
}

impl GlobalHistory {
    /// Get the global ops with sequence number larger than `seq`.
    fn ops_after(&self, seq: GlobalSeq) -> Vec<FatOp> {
        if seq < self.global.len() as GlobalSeq {
            self.global[(seq as usize)..].to_vec()
        } else {
            vec![]
        }
    }

    /// Add a new local `op` to the local history.
    /// Add it to the correct context group.
    fn add_local_op(&mut self, op: FatOp) {
        let current_global_seq = self.global.len() as u32;
        if self.local.len() == 0 {
            self.local.push(ContextOps {
                context: current_global_seq,
                ops: vec![op],
            });
        } else {
            let last_context = self.local.last().unwrap().context;
            if last_context == current_global_seq {
                self.local.last_mut().unwrap().ops.push(op);
            } else {
                self.local.push(ContextOps {
                    context: current_global_seq,
                    ops: vec![op],
                });
            }
        }
    }

    /// If there is at least one op in the local history, return a
    /// reference to the first one.
    fn peek_first_local_op(&self) -> Option<&FatOp> {
        self.local.first().map(|context_ops| &context_ops.ops[0])
    }

    /// If there is at least one op in the local history, return a
    /// reference to the last one.
    fn peek_last_local_op(&self) -> Option<&FatOp> {
        self.local
            .last()
            .map(|context_ops| &context_ops.ops[context_ops.ops.len() - 1])
    }

    /// If there is at least one op in the local history, pop it and
    /// return it.
    fn pop_first_local_op(&mut self) -> Option<FatOp> {
        if self.local.len() == 0 {
            None
        } else {
            // It's ok that we pop ops from local history one-by-one,
            // rather than popping all the ops in the same context op
            // together. Because when we send the ops to the server,
            // we send ops in the same context together, when the
            // server send them back, they must be still together. So
            // what will happen is that all the ops in the same
            // context are popped continuously one-by-one. No danger
            // of funky things happening.
            let ops = &mut self.local[0].ops;
            let op = ops.remove(0);
            if ops.len() == 0 {
                self.local.remove(0);
            }
            Some(op)
        }
    }

    pub fn process_opkind(&mut self, kind: EditorOpKind) -> EngineResult<OpKind> {
        let seq_of_new_op = self.global.len() as usize + self.local.len() + 1 + 1;
        match kind {
            EditorOpKind::Original => {
                // If the editor undone an op and makes an original
                // edit, the undone op is lost (can't be redone
                // anymore).
                if let Some(idx) = self.undo_tip {
                    self.undo_queue.drain(idx..);
                }
                Ok(OpKind::Original)
            }
            EditorOpKind::Undo => {
                // If this op is an undo op, find the original op it's
                // supposed to undo, and calculate the delta.
                let seq_of_orig;
                if let Some(idx) = self.undo_tip {
                    if idx == 0 {
                        return Err(EngineError::UndoError("No more op to undo".to_string()));
                    }
                    seq_of_orig = self.undo_queue[idx - 1];
                    self.undo_tip = Some(idx - 1);
                } else {
                    if self.undo_queue.len() == 0 {
                        return Err(EngineError::UndoError("No more op to undo".to_string()));
                    }
                    seq_of_orig = *self.undo_queue.last().unwrap();
                    self.undo_tip = Some(self.undo_queue.len() - 1);
                }
                Ok(OpKind::Undo(seq_of_new_op - seq_of_orig as usize))
            }
            EditorOpKind::Redo => {
                if let Some(idx) = self.undo_tip {
                    let seq_of_inverse = self.undo_queue[idx];
                    if idx == self.undo_queue.len() - 1 {
                        self.undo_tip = None;
                    } else {
                        self.undo_tip = Some(idx + 1);
                    }
                    Ok(OpKind::Undo(seq_of_new_op - seq_of_inverse as usize))
                } else {
                    Err(EngineError::UndoError("No ops to redo".to_string()))
                }
            }
        }
    }
}

/// OT control algorithm engine for client. Used by
/// [crate::collab_client::Doc].
#[derive(Debug, Clone)]
pub struct ClientEngine {
    /// History storing the global timeline (seen by the server).
    gh: GlobalHistory,
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
    /// The largest global sequence number we've assigned.
    current_seq: GlobalSeq,
}

impl ClientEngine {
    /// Return the site id.
    pub fn site_id(&self) -> SiteId {
        self.site.clone()
    }
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
    SiteSeqMismatch(LocalSeq, FatOp),
    #[error("Op {0:?} should have a global seq number, but doesn't")]
    SeqMissing(FatOp),
    #[error("Undo error: {0}")]
    UndoError(String),
}

type EngineResult<T> = Result<T, EngineError>;

// *** Processing functions

impl ClientEngine {
    pub fn new(site: SiteId) -> ClientEngine {
        ClientEngine {
            gh: GlobalHistory {
                global: vec![],
                local: vec![],
                undo_queue: vec![],
                undo_tip: None,
            },
            site,
            current_seq: 0,
            current_site_seq: 0,
            last_site_seq_sent_out: 0,
        }
    }

    /// Return whether the previous ops we sent to the server have
    /// been acked.
    fn prev_op_acked(&self) -> bool {
        if let Some(op) = self.gh.peek_first_local_op() {
            if op.site_seq == self.last_site_seq_sent_out + 1 {
                true
            } else {
                false
            }
        } else {
            // This value doesn't matter, if local is empty, we are
            // not sending out anything anyway.
            false
        }
    }

    /// Return packaged local ops if it's appropriate time to send
    /// them out, return None if there's no pending local ops or it's
    /// not time. (Client can only send out new local ops when
    /// previous in-flight local ops are acked by the server.) The
    /// returned ops must be sent to server for engine to work right.
    pub fn maybe_package_local_ops(&mut self) -> Option<Vec<ContextOps>> {
        if !self.prev_op_acked() {
            return None;
        }
        let context_ops = self.gh.local.clone();
        if context_ops.len() == 0 {
            return None;
        }
        let op = self.gh.peek_last_local_op().unwrap();
        self.last_site_seq_sent_out = op.site_seq;
        Some(context_ops)
    }

    /// Process local op, possibly transform it and add it to history.
    pub fn process_local_op(&mut self, mut op: FatOp, kind: EditorOpKind) -> EngineResult<()> {
        log::debug!(
            "process_local_op({:?}) current_site_seq: {}",
            &op,
            self.current_site_seq
        );

        if op.site_seq != self.current_site_seq + 1 {
            return Err(EngineError::SiteSeqMismatch(self.current_site_seq + 1, op));
        }
        self.current_site_seq = op.site_seq;

        op.kind = self.gh.process_opkind(kind)?;

        self.gh.add_local_op(op);
        Ok(())
    }

    /// Process remote op, transform it and add it to history. Return
    /// the ops for the editor to apply, if any. When calling this
    /// function repeatedly to process a batch of ops that came from
    /// the server, make sure you don't all other function in between.
    pub fn process_remote_op(&mut self, mut op: FatOp) -> EngineResult<Option<FatOp>> {
        log::debug!(
            "process_remote_op({:?}) current_seq: {}",
            &op,
            self.current_seq
        );

        let seq = op.seq.ok_or_else(|| EngineError::SeqMissing(op.clone()))?;
        if seq != self.current_seq + 1 {
            return Err(EngineError::SeqMismatch(self.current_seq + 1, op.clone()));
        }

        if op.site == self.site {
            // In global history, move the op from the local part to
            // the global part.
            let local_op = self.gh.pop_first_local_op();
            if local_op.is_none() {
                return Err(EngineError::OpMissing(op.clone()));
            }
            let local_op = local_op.unwrap();
            if local_op.site != op.site || local_op.site_seq != op.site_seq {
                return Err(EngineError::OpMismatch(op.clone(), local_op.clone()));
            }
            self.current_seq = seq;
            self.gh.global.push(op);
            Ok(None)
        } else {
            // We received an op generated at another site, transform
            // it, add it to history, and return it.
            //
            // Why do we transform each context group from scratch
            // every time? This way we can properly skip over
            // original-inverse pairs in the remote ops.
            if self.gh.local.len() > 0 {
                let first_context = self.gh.local[0].context;
                let mut global_ops = self.gh.ops_after(first_context);
                global_ops.push(op);

                for context_ops in &self.gh.local {
                    // Suppose global history is [1 2 A], op is B.
                    // Suppose context_ops = [3 4], and context = 2,
                    // meaning context set C(3) = [1 2].
                    global_ops = ops_after(global_ops, context_ops.context)?;
                    // Now global_ops is [A B]. Both context_ops and
                    // global_ops has context = [1 2].
                    (global_ops, _) = quatradic_transform(global_ops, context_ops.ops.clone());
                    // Now global_ops = [A' B'] has context = [1 2 3
                    // 4], and is suitable for transforming with the
                    // next context group.
                }

                // Now op is properly transformed.
                op = global_ops.remove(global_ops.len() - 1);
            };

            self.current_seq = seq;
            self.gh.global.push(op.clone());

            // Update undo delta in local history.
            let mut offset = 0;
            for context_ops in &mut self.gh.local {
                for op in &mut context_ops.ops {
                    let kind = op.kind;
                    if let OpKind::Undo(delta) = kind {
                        if offset < delta {
                            op.kind = OpKind::Undo(delta + 1);
                        }
                    }
                    offset += 1;
                }
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

            Ok(Some(op))
        }
    }

    /// Generate an undo op from the current undo tip. Return None if
    /// there are no more ops to undo.
    pub fn generate_undo_op(&mut self) -> Option<Op> {
        todo!()
    }

    /// Generate a redo op from the current undo tip. Return None if
    /// there are no more ops to redo.
    pub fn generate_redo_op(&mut self) -> Option<Op> {
        todo!()
    }
}

impl ServerEngine {
    pub fn new() -> ServerEngine {
        ServerEngine {
            gh: GlobalHistory {
                global: vec![],
                local: vec![],
                undo_queue: vec![],
                undo_tip: None,
            },
            current_seq: 0,
        }
    }

    /// Process ops from a client, return the transformed ops.
    pub fn process_ops(&mut self, vec_context_ops: Vec<ContextOps>) -> EngineResult<Vec<FatOp>> {
        if vec_context_ops.len() == 0 {
            return Ok(vec![]);
        }
        let first_context = vec_context_ops[0].context;
        println!("first_context: {:?}", first_context);
        let mut l1 = self.gh.ops_after(first_context);
        println!("l1: {:?}", l1);
        let mut result_ops = vec![];
        // Suppose global ops is [1 2 A B C], and vec_context_ops =
        // [[3 4] [5 6]], where [3 4]'s context is 2, meaning its
        // context set is {1 2}, [5 6]'s context is B, meaning its
        // context set is {1 2 A B 3 4}. Then l1 = [A B C].
        for context_ops in vec_context_ops {
            println!("context: {:?}", context_ops.context);
            l1 = ops_after(l1, context_ops.context)?;
            println!("l1: {:?}", l1);
            let transformed_ops;
            (l1, transformed_ops) = quatradic_transform(l1, context_ops.ops);
            println!("l1': {:?} transformed_ops: {:?}", &l1, &transformed_ops);
            // Now l1 is [A' B' C'] whose context is {1 2 3 4},
            // meaning [C']'s context set is { 1 2 A B 3 4}, which is
            // suitable for transforming with [5 6] in the next
            // iteration.
            result_ops.extend(transformed_ops)
        }
        // Assign sequence number for each op in ops.
        for mut op in &mut result_ops {
            op.seq = Some(self.current_seq + 1);
            self.current_seq += 1;
        }
        self.gh.global.extend_from_slice(&result_ops[..]);
        Ok(result_ops)
    }
}

// *** Test

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::Op;

    fn apply(doc: &mut String, op: &Op) {
        match op {
            Op::Ins((pos, str)) => {
                doc.insert_str(*pos as usize, &str);
            }
            Op::Del(edits) => {
                for (pos, str) in edits.iter().rev() {
                    doc.replace_range((*pos as usize)..(*pos as usize + str.len()), "");
                }
            }
        }
    }

    fn make_fatop(op: Op, site: &SiteId, site_seq: LocalSeq) -> FatOp {
        FatOp {
            seq: None,
            site: site.clone(),
            site_seq,
            doc: 1,
            op,
            kind: OpKind::Original,
            group_seq: 1, // Dummy value.
        }
    }

    // **** Puzzles

    /// deOPT puzzle, take from II.c in "A Time Interval Based
    // Consistency Control Algorithm for Interactive Groupware
    // Applications".
    #[test]
    fn deopt_puzzle() {
        let mut doc_a = "abcd".to_string();
        let mut doc_b = "abcd".to_string();

        let site_a = 1;
        let site_b = 2;
        let mut client_a = ClientEngine::new(site_a.clone());
        let mut client_b = ClientEngine::new(site_b.clone());

        let mut server = ServerEngine::new();

        let op_a1 = make_fatop(Op::Del(vec![(0, "a".to_string())]), &site_a, 1);
        let op_a2 = make_fatop(Op::Ins((2, "x".to_string())), &site_a, 2);
        let op_b1 = make_fatop(Op::Del(vec![(2, "c".to_string())]), &site_b, 1);

        let kind = EditorOpKind::Original;

        // Local edits.
        apply(&mut doc_a, &op_a1.op);
        client_a.process_local_op(op_a1.clone(), kind).unwrap();
        assert_eq!(doc_a, "bcd");

        apply(&mut doc_a, &op_a2.op);
        client_a.process_local_op(op_a2.clone(), kind).unwrap();
        assert_eq!(doc_a, "bcxd");

        apply(&mut doc_b, &op_b1.op);
        client_b.process_local_op(op_b1.clone(), kind).unwrap();
        assert_eq!(doc_b, "abd");

        // Server processing.
        let ops_from_a = client_a.maybe_package_local_ops().unwrap();
        let ops_from_b = client_b.maybe_package_local_ops().unwrap();
        println!("ops_from_b: {:?}", &ops_from_b);
        let a_ops = server.process_ops(ops_from_a).unwrap();
        let b_ops = server.process_ops(ops_from_b).unwrap();
        println!("b_ops: {:?}", &b_ops);

        // Client A processing.
        let a1_at_a = client_a.process_remote_op(a_ops[0].clone()).unwrap();
        assert_eq!(a1_at_a, None);
        let a2_at_a = client_a.process_remote_op(a_ops[1].clone()).unwrap();
        assert_eq!(a2_at_a, None);
        let b1_at_a = client_a
            .process_remote_op(b_ops[0].clone())
            .unwrap()
            .unwrap();

        apply(&mut doc_a, &b1_at_a.op);
        assert_eq!(doc_a, "bxd");

        // Client B processing.
        let a1_at_b = client_b
            .process_remote_op(a_ops[0].clone())
            .unwrap()
            .unwrap();
        let a2_at_b = client_b
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

    // False-tie puzzle, take from II.D in "A Time Interval Based
    // Consistency Control Algorithm for Interactive Groupware
    // Applications".
    #[test]
    fn false_tie_puzzle() {
        let mut doc_a = "abc".to_string();
        let mut doc_b = "abc".to_string();
        let mut doc_c = "abc".to_string();

        let site_a = 1;
        let site_b = 2;
        let site_c = 3;
        let mut client_a = ClientEngine::new(site_a.clone());
        let mut client_b = ClientEngine::new(site_b.clone());
        let mut client_c = ClientEngine::new(site_c.clone());

        let mut server = ServerEngine::new();

        let op_a1 = make_fatop(Op::Ins((2, "1".to_string())), &site_a, 1);
        let op_b1 = make_fatop(Op::Ins((1, "2".to_string())), &site_b, 1);
        let op_c1 = make_fatop(Op::Del(vec![(1, "b".to_string())]), &site_c, 1);

        let kind = EditorOpKind::Original;

        // Local edits.
        apply(&mut doc_a, &op_a1.op);
        client_a.process_local_op(op_a1.clone(), kind).unwrap();
        assert_eq!(doc_a, "ab1c");

        apply(&mut doc_b, &op_b1.op);
        client_b.process_local_op(op_b1.clone(), kind).unwrap();
        assert_eq!(doc_b, "a2bc");

        apply(&mut doc_c, &op_c1.op);
        client_c.process_local_op(op_c1.clone(), kind).unwrap();
        assert_eq!(doc_c, "ac");

        // Server processing.
        let ops_from_a = client_a.maybe_package_local_ops().unwrap();
        let ops_from_b = client_b.maybe_package_local_ops().unwrap();
        let ops_from_c = client_c.maybe_package_local_ops().unwrap();

        let b_ops = server.process_ops(ops_from_b).unwrap();
        let c_ops = server.process_ops(ops_from_c).unwrap();
        let a_ops = server.process_ops(ops_from_a).unwrap();

        // Client A processing.
        let b1_at_a = client_a
            .process_remote_op(b_ops[0].clone())
            .unwrap()
            .unwrap();
        let c1_at_a = client_a
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
        let c1_at_b = client_b
            .process_remote_op(c_ops[0].clone())
            .unwrap()
            .unwrap();
        let a1_at_b = client_b
            .process_remote_op(a_ops[0].clone())
            .unwrap()
            .unwrap();
        assert_eq!(b1_at_b, None);

        apply(&mut doc_b, &c1_at_b.op);
        assert_eq!(doc_b, "a2c");
        apply(&mut doc_b, &a1_at_b.op);
        assert_eq!(doc_b, "a21c");

        // Client C processing.
        let b1_at_c = client_c
            .process_remote_op(b_ops[0].clone())
            .unwrap()
            .unwrap();
        let c1_at_c = client_c.process_remote_op(c_ops[0].clone()).unwrap();
        let a1_at_c = client_c
            .process_remote_op(a_ops[0].clone())
            .unwrap()
            .unwrap();
        assert_eq!(c1_at_c, None);

        apply(&mut doc_c, &b1_at_c.op);
        assert_eq!(doc_c, "a2c");
        apply(&mut doc_c, &a1_at_c.op);
        assert_eq!(doc_c, "a21c");
    }
}
