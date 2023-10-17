//! This module provides [ClientEngine] and [ServerEngine] that
//! implements the client and server part of the OT control algorithm.

use crate::{op::quatradic_transform, types::*};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

/// A range in the full document. Each range is either a live text
/// (not deleted) or dead text (deleted). All the ranges should be in
/// order, connect, and don't overlap.
#[derive(Debug, Clone)]
enum Range {
    /// Live text, (pos, len).
    Live(u64, u64),
    /// Tombstone, (pos, text).
    Dead(u64, String),
}

/// A cursor in the full document (which contains tombstones). This
/// cursor also tracks the editor document's position. Translating
/// between editor position and full document position around a cursor
/// is very fast.
#[derive(Debug, Clone)]
struct Cursor {
    /// The position of this cursor in the editor's document. This
    /// position doesn't account for the tombstones.
    editor_pos: u64,
    /// The actual position in the full document containing tombstones.
    full_pos: u64,
    /// The index of the nearest tombestone in the graveyard that's
    /// positioned after the cursor.
    tombestone_idx: Option<usize>,
}

/// The full document that contains both live text and tombstones.
#[derive(Debug, Clone)]
struct FullDoc {
    ranges: Vec<Range>,
    /// Cursor for each site.
    cursors: HashMap<SiteId, Cursor>,
}

impl Default for FullDoc {
    fn default() -> Self {
        FullDoc {
            ranges: vec![],
            cursors: HashMap::new(),
        }
    }
}

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
    /// The full document.
    full_doc: FullDoc,
}

impl Default for GlobalHistory {
    fn default() -> Self {
        GlobalHistory {
            global: vec![],
            local: vec![],
            undo_queue: vec![],
            undo_tip: None,
            full_doc: FullDoc::default(),
        }
    }
}

// *** GlobalHistory

impl GlobalHistory {
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
                    if idx == self.undo_queue.len() - 1 {
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

// *** ClientEngine DO

impl ClientEngine {
    pub fn new(site: SiteId, base_seq: GlobalSeq) -> ClientEngine {
        ClientEngine {
            gh: GlobalHistory::default(),
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

        let inferred_seq = (self.gh.global.len() + self.gh.local.len() + 1) as GlobalSeq;
        op.kind = self.gh.process_opkind(kind, inferred_seq)?;

        match &op.kind {
            OpKind::Original => self.gh.undo_queue.push(inferred_seq),
            OpKind::Undo(_) => (),
        }

        self.gh.local.push(op);

        Ok(())
    }

    /// Process remote op, transform it and add it to history. Return
    /// the ops for the editor to apply, if any.
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
            // Move the op from the local part to the global part.
            if self.gh.local.len() == 0 {
                return Err(EngineError::OpMissing(op.clone()));
            }
            let local_op = self.gh.local.remove(0);
            if local_op.site != op.site || local_op.site_seq != op.site_seq {
                return Err(EngineError::OpMismatch(op.clone(), local_op.clone()));
            }
            self.current_seq = seq;
            self.gh.global.push(op);
            Ok(None)
        } else {
            // We received an op generated at another site, transform
            // the local history against it, add it to history, and
            // return it.
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

            Ok(Some(op))
        }
    }
}

// *** ClientEngine UNDO

impl ClientEngine {
    /// Generate undo or redo ops from the current undo tip.
    fn generate_undo_op_1(&mut self, redo: bool) -> Vec<EditorOp> {
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
        ops.into_iter().map(|op| op.op.into()).collect()
    }

    /// Generate an undo op from the current undo tip. Return an empty
    /// vector if there's no more op to undo.
    pub fn generate_undo_op(&mut self) -> Vec<EditorOp> {
        self.generate_undo_op_1(false)
    }

    /// Generate a redo op from the current undo tip. Return en empty
    /// vector if there's no more ops to redo.
    pub fn generate_redo_op(&mut self) -> Vec<EditorOp> {
        self.generate_undo_op_1(true)
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
    pub fn new() -> ServerEngine {
        ServerEngine {
            gh: GlobalHistory::default(),
            current_seq: 0,
        }
    }

    /// Process `op` from a client, return the transformed `ops`.
    pub fn process_ops(
        &mut self,
        mut ops: Vec<FatOp>,
        context: GlobalSeq,
    ) -> EngineResult<Vec<FatOp>> {
        let l1 = self.gh.ops_after(context);
        // Transform ops against L1, then we can append ops to global
        // history. `quatradic_transform` can skip coupled
        // original-inverse pairs but can't take care of decoupled
        // ones. But we don't care. It's too expensive to handle
        // decoupled original-inverse pairs.
        (_, ops) = quatradic_transform(l1, ops);

        // Assign sequence number for each op in ops.
        for mut op in &mut ops {
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
    use crate::op::Op;

    fn apply(doc: &mut String, op: &Op) {
        match op {
            Op::Ins((pos, str)) => {
                doc.insert_str(*pos as usize, &str);
            }
            Op::Del(edits, _) => {
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
        let mut client_a = ClientEngine::new(site_a.clone(), 0);
        let mut client_b = ClientEngine::new(site_b.clone(), 0);

        let mut server = ServerEngine::new();

        let op_a1 = make_fatop(Op::Del(vec![(0, "a".to_string())], vec![]), &site_a, 1);
        let op_a2 = make_fatop(Op::Ins((2, "x".to_string())), &site_a, 2);
        let op_b1 = make_fatop(Op::Del(vec![(2, "c".to_string())], vec![]), &site_b, 1);

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
        let a_ops = server
            .process_ops(ops_from_a.ops, ops_from_a.context)
            .unwrap();
        let b_ops = server
            .process_ops(ops_from_b.ops, ops_from_b.context)
            .unwrap();
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
        let mut client_a = ClientEngine::new(site_a.clone(), 0);
        let mut client_b = ClientEngine::new(site_b.clone(), 0);
        let mut client_c = ClientEngine::new(site_c.clone(), 0);

        let mut server = ServerEngine::new();

        let op_a1 = make_fatop(Op::Ins((2, "1".to_string())), &site_a, 1);
        let op_b1 = make_fatop(Op::Ins((1, "2".to_string())), &site_b, 1);
        let op_c1 = make_fatop(Op::Del(vec![(1, "b".to_string())], vec![]), &site_c, 1);

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

    /// This is what happens when you insert "{" in Emacs.
    #[test]
    fn undo() {
        let site_id = 1;
        let mut client_engine = ClientEngine::new(site_id, 1);
        // op1: original edit.
        let op1 = make_fatop(Op::Ins((0, "{".to_string())), &site_id, 1);
        // Auto-insert parenthesis. I don't know why it deletes the
        // inserted bracket first, but that's what it does.
        let op2 = make_fatop(Op::Del(vec![(0, "{".to_string())], vec![]), &site_id, 2);
        let op3 = make_fatop(Op::Ins((0, "{".to_string())), &site_id, 3);
        let op4 = make_fatop(Op::Ins((1, "}".to_string())), &site_id, 4);
        // All four ops have the same group, which is what we want.

        client_engine
            .process_local_op(op1, EditorOpKind::Original)
            .unwrap();
        client_engine
            .process_local_op(op2, EditorOpKind::Original)
            .unwrap();
        client_engine
            .process_local_op(op3, EditorOpKind::Original)
            .unwrap();
        client_engine
            .process_local_op(op4, EditorOpKind::Original)
            .unwrap();

        let undo_ops = client_engine.generate_undo_op();

        println!("undo_ops: {:?}", undo_ops);

        assert!(undo_ops[0] == Op::Del(vec![(1, "}".to_string())], vec![]));
        assert!(undo_ops[1] == Op::Del(vec![(0, "{".to_string())], vec![]));
        assert!(undo_ops[2] == Op::Ins((0, "{".to_string())));
        assert!(undo_ops[3] == Op::Del(vec![(0, "{".to_string())], vec![]));
    }
}
