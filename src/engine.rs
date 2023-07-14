use crate::types::*;
use serde::{Deserialize, Serialize};
use std::result::Result;
use thiserror::Error;

// *** Data structure

/// A continuous series of local ops with a context. The context
/// sequence represents the context on which the first local op in the
/// series is based: all the global ops with a sequence number leq to
/// the context seq number. The rest local ops are based on the
/// context plus precious ops in the series.
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

/// The history buffer that reflects the history seen by the editor.
#[derive(Debug, Clone)]
pub struct EditorHistory {
    /// History buffer.
    history: Vec<LeanOp>,
    // /// The index of the last local-turned global op in the history.
    // /// Any local op is after this index.
    // last_sequenced_idx: usize,
    /// Indexes of local, original ops in `history`: the ops to undo.
    /// Since this is a linear history, some original ops might be
    /// skipped when the user undo and then generates fresh edits.
    orig_history: Vec<usize>,
    /// Index into `orig_history`. It points to the original op we
    /// just undone when it's not None.
    undo_tip: Option<usize>,
}

impl EditorHistory {
    /// Mark the identity flag of the op at `tip` as `identify`. Note
    /// that `tip` is an index of the `orig_history` field, not
    /// `history` field!! After marking the identify flag, transform
    /// ops after the marked op in `history` such that the effect of
    /// the op is removed/added (depending on the value of `identity`).
    fn mark_undo_tip(&mut self, tip: usize, identity: bool) -> EngineResult<()> {
        let idx = self.orig_history[tip];
        {
            let op = &mut self.history[idx];
            if op.identity == identity {
                if op.identity {
                    return Err(EngineError::UndoError(
                        "Undoing an op that's already undone".to_string(),
                    ));
                } else {
                    return Err(EngineError::UndoError(
                        "Redoing an op that's already redone".to_string(),
                    ));
                }
            }
            op.identity = identity;
        }
        let mut new_op = self.history[idx].clone();
        if identity {
            new_op.op = new_op.op.inverse();
            new_op.identity = false;
        }
        let ops = &self.history[idx + 1..];
        let new_ops = new_op.symmetric_transform(ops);
        self.history.splice(idx + 1.., new_ops);
        Ok(())
    }

    /// Process the new local `op`.
    fn process_local_op(&mut self, op: &FatOp, kind: OpKind) -> EngineResult<()> {
        match kind {
            OpKind::Original => {
                self.history.push(LeanOp::new(&op.op, &op.site));
                let index = self.history.len() - 1;

                // Trim off the undone ops: now that the user have
                // made an original op, those undone ops can never be
                // redone.
                if let Some(tip) = self.undo_tip {
                    self.orig_history.truncate(tip);
                }
                self.orig_history.push(index);
                self.undo_tip = None;
            }
            OpKind::Undo => {
                if let Some(tip) = self.undo_tip {
                    if tip == 0 {
                        return Err(EngineError::UndoError("No more to undo".to_string()));
                    }
                    self.undo_tip = Some(tip - 1);
                } else {
                    self.undo_tip = Some(self.orig_history.len() - 1)
                }
                self.mark_undo_tip(self.undo_tip.unwrap(), true)?;
            }
            OpKind::Redo => {
                if self.undo_tip.is_none() {
                    return Err(EngineError::UndoError("No more to redo".to_string()));
                }
                let tip = self.undo_tip.unwrap();
                self.mark_undo_tip(tip, false)?;
                self.undo_tip = if tip + 1 == self.orig_history.len() {
                    None
                } else {
                    Some(tip + 1)
                };
            }
        }
        Ok(())
    }

    fn process_remote_op(&mut self, op: &FatOp) {
        self.history.push(LeanOp::new(&op.op, &op.site));
    }

    /// Generate an undo op from the current undo tip. Return None if
    /// there are no more ops to undo.
    fn generate_undo_op(&mut self) -> Option<Op> {
        let orig_ops = &self.orig_history;
        let ops = &self.history;
        log::debug!("gen undo: {:?}", &ops);
        if orig_ops.len() == 0 {
            return None;
        }
        let mut idx = self.undo_tip.or_else(|| Some(orig_ops.len())).unwrap();
        if idx == 0 {
            None
        } else {
            idx -= 1;
            log::debug!("idx: {}, orig_history: {:?}", idx, orig_ops);
            let mut op = ops[orig_ops[idx]].clone();
            log::debug!("op: {:?}", op);
            op.op = op.op.inverse();
            let _ = op.symmetric_transform(&ops[orig_ops[idx] + 1..]);
            Some(op.op)
        }
    }

    /// Generate a redo op from the current undo tip. Return None if
    /// there are no more ops to redo.
    fn generate_redo_op(&mut self) -> Option<Op> {
        if self.undo_tip.is_none() {
            return None;
        }
        let orig_ops = &self.orig_history;
        let ops = &self.history;
        let idx = self.undo_tip.unwrap();
        let mut op = ops[orig_ops[idx]].clone();
        // Use the original op rather than the transformed op.
        // If everything goes right, there is no undone op
        // before this op in the editor history, so using the
        // original op is ok.
        op.op = op.orig.clone();
        let _ = op.symmetric_transform(&ops[orig_ops[idx] + 1..]);
        return Some(op.op);
    }
}

/// Global history buffer. The whole buffer is made of global_buffer +
/// local_buffer.
#[derive(Debug, Clone)]
pub struct GlobalHistory {
    global: Vec<FatOp>,
    local: Vec<FatOp>,
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
}

/// OT control algorithm engine for client.
#[derive(Debug, Clone)]
pub struct ClientEngine {
    /// History storing the global timeline (seen by the server).
    gh: GlobalHistory,
    /// History storing the editor's timeline (seen by the editor).
    eh: EditorHistory,
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

/// OT control algorithm engine for server.
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
            },
            eh: EditorHistory {
                history: vec![],
                // last_sequenced_idx: 0,
                orig_history: vec![],
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
        if self.gh.local.len() == 0 {
            // This value doesn't matter, if local is empty, we are
            // not sending out anything anyway.
            true
        } else if self.gh.local[0].site_seq == self.last_site_seq_sent_out + 1 {
            true
        } else {
            false
        }
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
    pub fn process_local_op(&mut self, op: FatOp, kind: OpKind) -> EngineResult<()> {
        log::debug!(
            "process_local_op({:?}) current_site_seq: {}",
            &op,
            self.current_site_seq
        );

        if op.site_seq != self.current_site_seq + 1 {
            return Err(EngineError::SiteSeqMismatch(self.current_site_seq + 1, op));
        }
        self.current_site_seq = op.site_seq;
        self.eh.process_local_op(&op, kind)?;
        self.gh.local.push(op);
        Ok(())
    }

    /// Process remote op, possibly transform it and add it to history.
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
            // it, add it to history, and return it.
            let new_local_ops = op.symmetric_transform(&self.gh.local[..]);
            self.current_seq = seq;
            self.gh.local = new_local_ops;
            self.gh.global.push(op.clone());
            self.eh.process_remote_op(&op);
            Ok(Some(op))
        }
    }

    /// Generate an undo op from the current undo tip. Return None if
    /// there are no more ops to undo.
    pub fn generate_undo_op(&mut self) -> Option<Op> {
        self.eh.generate_undo_op()
    }

    /// Generate a redo op from the current undo tip. Return None if
    /// there are no more ops to redo.
    pub fn generate_redo_op(&mut self) -> Option<Op> {
        self.eh.generate_redo_op()
    }
}

impl ServerEngine {
    pub fn new() -> ServerEngine {
        ServerEngine {
            gh: GlobalHistory {
                global: vec![],
                local: vec![],
            },
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
        // history.
        for mut l1_op in l1 {
            ops = l1_op.symmetric_transform(&ops);
        }
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
    use rand::prelude::*;
    use std::sync::mpsc;

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

        let kind = OpKind::Original;

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

        let kind = OpKind::Original;

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

    // **** Simulation

    struct Simulator {
        /// N documents.
        docs: Vec<String>,
        /// N clients.
        clients: Vec<ClientEngine>,
        /// N site sequences,
        site_seqs: Vec<LocalSeq>,
        /// The server.
        server: ServerEngine,
        /// N connections from server to client.
        conn_to_client: Vec<(mpsc::Sender<FatOp>, mpsc::Receiver<FatOp>)>,
        /// N connections from client to server. N tx and one rx.
        conn_to_server: Vec<(mpsc::Sender<ContextOps>, mpsc::Receiver<ContextOps>)>,
    }

    enum SimAction {
        // Client n makes an edit.
        Edit(usize, Op),
        // Server receives one op from client n and processes it.
        ServerReceive(usize),
        // Client n receives an op from server.
        ClientReceive(usize),
    }

    impl Simulator {
        /// Return a new simulator with `clients` clients that will
        /// make `n_edits` edits uniformly across all clients.
        fn new(n_clients: usize) -> Simulator {
            let docs = (0..n_clients).map(|_| String::new()).collect();
            let clients = (0..n_clients)
                .map(|n| ClientEngine::new(n as SiteId))
                .collect();
            let site_seqs = (0..n_clients).map(|_n| 0).collect();
            let server = ServerEngine::new();
            let conn_to_client = (0..n_clients).map(|_| mpsc::channel()).collect();
            let conn_to_server = (0..n_clients).map(|_| mpsc::channel()).collect();
            Simulator {
                docs,
                clients,
                site_seqs,
                server,
                conn_to_client,
                conn_to_server,
            }
        }
    }

    /// Run a simulation that will generate `n_edits` edits.
    /// `edit_Frequency` is the ratio between generating edits and
    /// processing them, a value larger than 1 means edits are
    /// generated faster than being processed.
    fn run_simulation(n_edits: usize, edit_frequency: f64) {
        let n_clients = 3;
        let mut sim = Simulator::new(n_clients);
        let edit_ratio: f64 = edit_frequency * (1f64 / ((1f64 + n_clients as f64) * 2f64 + 1f64));
        let mut rng = rand::thread_rng();
        let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyz".chars().collect();

        loop {
            let draw: f64 = rand::random();
            if draw < edit_ratio {
                // Client generates an edit, processes it, and sends
                // it to server.
                let client_idx = rand::random::<usize>() % n_clients;
                let client = &mut sim.clients[client_idx];
                let doc = &sim.docs[client_idx];
                let op = if rand::random::<f64>() < 0.5 {
                    let pos = rand::random::<usize>() % doc.len();
                    Op::Ins((pos as u64, alphabet.choose(&mut rng).unwrap().to_string()))
                } else {
                    let pos = rand::random::<usize>() % (doc.len() - 1);
                    Op::Del(vec![(
                        pos as u64,
                        doc.chars().nth(pos).unwrap().to_string(),
                    )])
                };
                sim.site_seqs[client_idx] += 1;
                let fatop = make_fatop(op, &(client_idx as SiteId), sim.site_seqs[client_idx]);
                client.process_local_op(fatop, OpKind::Original).unwrap();

                if client.prev_op_acked() {
                    // TODO: unwrap?
                    let ops = client.package_local_ops().unwrap();
                    sim.conn_to_server[client_idx].0.send(ops).unwrap();
                }
            } else if draw < (1f64 - edit_ratio) / 2f64 {
                // Server receives an op from a random client.
                let mut pool: Vec<usize> = vec![];
                for idx in 0..n_clients {
                    if sim.conn_to_server[idx].1.try_iter().next().is_some() {
                        pool.push(idx);
                    }
                }
                let idx = *pool.choose(&mut rng).unwrap();
                let received_ops = sim.conn_to_server[idx].1.recv().unwrap();
                let processed_ops = sim
                    .server
                    .process_ops(received_ops.ops, received_ops.context)
                    .unwrap();
                for (tx, _) in &sim.conn_to_client {
                    for op in &processed_ops {
                        tx.send(op.clone()).unwrap();
                    }
                }
            } else {
                todo!()
            }
        }
    }
}
