use crate::op::*;
use std::{cmp::max, result::Result};
use thiserror::Error;

// *** Data structure

/// A continuous series of local ops with a context. The context
/// sequence represents the context on which the first local op in the
/// series is based: all the global ops with a sequence number leq to
/// the context seq number. The rest local ops are based on the
/// context plus precious ops in the series.
pub struct ContextOps<O> {
    context: GlobalSeq,
    ops: Vec<FatOp<O>>,
}

/// The history buffer that reflects the history seen by the editor.
#[derive(Debug, Clone)]
pub struct EditorHistory<O> {
    /// History buffer.
    history: Vec<FatOp<O>>,
    /// The index of the last local-turned global op in the history.
    /// Any local op is after this index.
    last_sequenced_idx: u64,
}

/// Global history buffer. The whole buffer is made of global_buffer +
/// local_buffer.
#[derive(Debug, Clone)]
pub struct GlobalHistory<O> {
    global: Vec<FatOp<O>>,
    local: Vec<FatOp<O>>,
}

impl<O: Operation> GlobalHistory<O> {
    /// Get the global ops with sequence number larger than `seq`.
    fn ops_after(&self, seq: GlobalSeq) -> Vec<FatOp<O>> {
        let len = self.global.len() as u64;
        if seq < len {
            let start = (max(seq, 1) as usize) - 1;
            self.global[start..].to_vec()
        } else {
            vec![]
        }
    }
}

/// OT control algorithm engine for client.
pub struct ClientEngine<O> {
    /// History storing the global timeline (seen by the server).
    gh: GlobalHistory<O>,
    /// History storing the editor's timeline (seen by the editor).
    eh: EditorHistory<O>,
    /// The id of this site.
    site: String,
    /// The largest global sequence number we've seen. Any remote op
    /// we receive should have sequence equal to this number plus one.
    current_seq: GlobalSeq,
    // The largest local sequence number we've seen. Any local op we
    // receive should be have local sequence equal to this number plus
    // one.
    current_site_seq: LocalSeq,
    /// The local sequence number of the last op acked by the server.
    /// Client can't send new ops before the server has acked the
    /// previous ops the client have sent (stop-and-wait).
    last_acked_site_seq: LocalSeq,
}

/// OT control algorithm engine for server.
pub struct ServerEngine<O> {
    /// Global history.
    gh: GlobalHistory<O>,
    /// The largest global sequence number we've assigned.
    current_seq: GlobalSeq,
}

#[derive(Debug, Clone, Error)]
pub enum EngineError<O> {
    #[error("An op that we expect to exist isn't there")]
    OpMissing(FatOp<O>),
    #[error("We expect to find the first Op, but find the second instead")]
    OpMismatch(FatOp<O>, FatOp<O>),
    #[error("We expect to see this global sequence number in the op")]
    SeqMismatch(GlobalSeq, FatOp<O>),
    #[error("We expect to see this site sequence number in the op")]
    SiteSeqMismatch(LocalSeq, FatOp<O>),
    #[error("The op should have a global seq number, but doesn't")]
    SeqMissing(FatOp<O>),
}

type EngineResult<T, O> = Result<T, EngineError<O>>;

// *** Processing functions

impl<O: Operation> ClientEngine<O> {
    pub fn new(site: &SiteId) -> ClientEngine<O> {
        ClientEngine {
            gh: GlobalHistory {
                global: vec![],
                local: vec![],
            },
            eh: EditorHistory {
                history: vec![],
                last_sequenced_idx: 0,
            },
            site: site.clone(),
            current_seq: 0,
            current_site_seq: 0,
            last_acked_site_seq: 0,
        }
    }

    /// Return whether the previous ops we sent to the server have
    /// been acked.
    pub fn prev_op_acked(&self) -> bool {
        if self.current_site_seq == self.last_acked_site_seq {
            true
        } else {
            let op = &self.gh.local[0];
            self.last_acked_site_seq + 1 == op.site_seq
        }
    }

    /// Remove queuing local ops in the engine and return them.
    pub fn package_local_ops(&mut self) -> ContextOps<O> {
        assert!(self.prev_op_acked());
        let ops: Vec<FatOp<O>> = self.gh.local.drain(0..self.gh.local.len()).collect();
        ContextOps {
            context: self.current_seq,
            ops,
        }
    }

    /// Process local op, possibly transform it and add it to history.
    pub fn process_local_op(&mut self, op: FatOp<O>) -> EngineResult<(), O> {
        if op.site_seq != self.current_site_seq + 1 {
            return Err(EngineError::SiteSeqMismatch(self.current_site_seq + 1, op));
        }
        self.current_site_seq = op.site_seq;
        self.eh.history.push(op.clone());
        self.gh.local.push(op);
        Ok(())
    }

    /// Process remote op, possibly transform it and add it to history.
    pub fn process_remote_op(&mut self, mut op: FatOp<O>) -> EngineResult<Option<FatOp<O>>, O> {
        let seq = op.seq.ok_or_else(|| EngineError::SeqMissing(op.clone()))?;
        if seq != self.current_seq + 1 {
            return Err(EngineError::SeqMismatch(self.current_seq + 1, op.clone()));
        }

        if op.site == self.site {
            // The server has receive the local op we sent it, update
            // the seq number of this op in our history.
            for idx in (0..self.eh.history.len()).rev() {
                let local_op = &mut self.eh.history[idx];
                if local_op.site == op.site && local_op.site_seq == op.site_seq {
                    local_op.seq = op.seq;
                    self.eh.last_sequenced_idx = idx as u64;
                    break;
                }
            }
            // In global history, move the op from the local part to
            // the global part.
            if self.gh.local.len() == 0 {
                return Err(EngineError::OpMissing(op.clone()));
            }
            let local_op = self.gh.local.remove(0);
            if local_op.site != op.site || local_op.site_seq != op.site_seq {
                return Err(EngineError::OpMismatch(op.clone(), local_op.clone()));
            }
            self.last_acked_site_seq = op.site_seq;
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
            Ok(Some(op))
        }
    }
}

impl<O: Operation> ServerEngine<O> {
    pub fn new() -> ServerEngine<O> {
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
        mut ops: Vec<FatOp<O>>,
        context: GlobalSeq,
    ) -> EngineResult<Vec<FatOp<O>>, O> {
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
    use rand::prelude::*;
    use std::sync::mpsc;

    fn apply(doc: &mut String, op: &SimpleOp) {
        match op {
            SimpleOp::Ins(pos, char) => {
                doc.insert_str(*pos as usize, &char.to_string());
            }
            SimpleOp::Del(pos, char) => {
                if let Some(_) = char {
                    doc.replace_range((*pos as usize)..(*pos as usize + 1), "")
                }
            }
        }
    }

    fn make_fatop<O: Operation>(op: O, site: &SiteId, site_seq: LocalSeq) -> FatOp<O> {
        FatOp {
            seq: None,
            site: site.to_string(),
            site_seq,
            doc: "".to_string(),
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

        let site_a = "A".to_string();
        let site_b = "B".to_string();
        let mut client_a = ClientEngine::<SimpleOp>::new(&site_a);
        let mut client_b = ClientEngine::<SimpleOp>::new(&site_b);

        let mut server = ServerEngine::<SimpleOp>::new();

        let op_a1 = make_fatop(SimpleOp::Del(0, Some('a')), &site_a, 1);
        let op_a2 = make_fatop(SimpleOp::Ins(2, 'x'), &site_a, 2);
        let op_b1 = make_fatop(SimpleOp::Del(2, Some('c')), &site_b, 1);

        // Local edits.
        apply(&mut doc_a, &op_a1.op);
        client_a.process_local_op(op_a1.clone()).unwrap();
        assert_eq!(doc_a, "bcd");

        apply(&mut doc_a, &op_a2.op);
        client_a.process_local_op(op_a2.clone()).unwrap();
        assert_eq!(doc_a, "bcxd");

        apply(&mut doc_b, &op_b1.op);
        client_b.process_local_op(op_b1.clone()).unwrap();
        assert_eq!(doc_b, "abd");

        // Server processing.
        let a_ops = server.process_ops(vec![op_a1, op_a2], 0).unwrap();
        let b_ops = server.process_ops(vec![op_b1], 0).unwrap();

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

        let site_a = "A".to_string();
        let site_b = "B".to_string();
        let site_c = "C".to_string();
        let mut client_a = ClientEngine::<SimpleOp>::new(&site_a);
        let mut client_b = ClientEngine::<SimpleOp>::new(&site_b);
        let mut client_c = ClientEngine::<SimpleOp>::new(&site_c);

        let mut server = ServerEngine::<SimpleOp>::new();

        let op_a1 = make_fatop(SimpleOp::Ins(2, '1'), &site_a, 1);
        let op_b1 = make_fatop(SimpleOp::Ins(1, '2'), &site_b, 1);
        let op_c1 = make_fatop(SimpleOp::Del(1, Some('b')), &site_c, 1);

        // Local edits.
        apply(&mut doc_a, &op_a1.op);
        client_a.process_local_op(op_a1.clone()).unwrap();
        assert_eq!(doc_a, "ab1c");

        apply(&mut doc_b, &op_b1.op);
        client_b.process_local_op(op_b1.clone()).unwrap();
        assert_eq!(doc_b, "a2bc");

        apply(&mut doc_c, &op_c1.op);
        client_c.process_local_op(op_c1.clone()).unwrap();
        assert_eq!(doc_c, "ac");

        // Server processing.
        let b_ops = server.process_ops(vec![op_b1], 0).unwrap();
        let c_ops = server.process_ops(vec![op_c1], 0).unwrap();
        let a_ops = server.process_ops(vec![op_a1], 0).unwrap();

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

    struct Simulator<O> {
        /// N documents.
        docs: Vec<String>,
        /// N clients.
        clients: Vec<ClientEngine<O>>,
        /// N site sequences,
        site_seqs: Vec<LocalSeq>,
        /// The server.
        server: ServerEngine<O>,
        /// N connections from server to client.
        conn_to_client: Vec<(mpsc::Sender<FatOp<O>>, mpsc::Receiver<FatOp<O>>)>,
        /// N connections from client to server. N tx and one rx.
        conn_to_server: Vec<(mpsc::Sender<ContextOps<O>>, mpsc::Receiver<ContextOps<O>>)>,
    }

    enum SimAction<O> {
        // Client n makes an edit.
        Edit(usize, O),
        // Server receives one op from client n and processes it.
        ServerReceive(usize),
        // Client n receives an op from server.
        ClientReceive(usize),
    }

    impl<O: Operation> Simulator<O> {
        /// Return a new simulator with `clients` clients that will
        /// make `n_edits` edits uniformly across all clients.
        fn new(n_clients: usize) -> Simulator<O> {
            let docs = (0..n_clients).map(|_| String::new()).collect();
            let clients = (0..n_clients)
                .map(|n| ClientEngine::new(&n.to_string()))
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
        let mut sim = Simulator::<SimpleOp>::new(n_clients);
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
                    SimpleOp::Ins(pos as u64, alphabet.choose(&mut rng).unwrap().clone())
                } else {
                    let pos = rand::random::<usize>() % (doc.len() - 1);
                    SimpleOp::Del(pos as u64, Some(doc.chars().nth(pos).unwrap()))
                };
                sim.site_seqs[client_idx] += 1;
                let fatop = make_fatop(op, &client_idx.to_string(), sim.site_seqs[client_idx]);
                client.process_local_op(fatop).unwrap();

                if client.prev_op_acked() {
                    let ops = client.package_local_ops();
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
