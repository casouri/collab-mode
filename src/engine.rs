use std::{result::Result, cmp::max};
use thiserror::Error;
use crate::op::*;

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
    #[error("We expect to see this sequence number in the op")]
    SeqMismatch(GlobalSeq, FatOp<O>),
    #[error("The op should have a global seq number, but doesn't")]
    SeqMissing(FatOp<O>),
}

type EngineResult<T, O> = Result<T, EngineError<O>>;

impl<O: Operation> ClientEngine<O> {

    fn new(site: &SiteId) -> ClientEngine<O> {
        ClientEngine {
            gh: GlobalHistory { global: vec![], local: vec![], },
            eh: EditorHistory { history: vec![], last_sequenced_idx: 0 },
            site: site.clone(),
            current_seq: 0,
        }
    }

    /// Process local op, possibly transform it and add it to history.
    fn process_local_op(&mut self, op: FatOp<O>) {
        self.eh.history.push(op.clone());
        self.gh.local.push(op);
    }

    /// Process remote op, possibly transform it and add it to history.
    fn process_remote_op(&mut self, mut op: FatOp<O>) -> EngineResult<Option<FatOp<O>>, O> {
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
            self.gh.global.push(op);
            self.current_seq = seq;
            Ok(None)
        } else {
            // We received an op generated at another site, transform
            // it, add it to history, and return it.
            let new_local_ops = op.symmetric_transform(&self.gh.local[..]);
            self.gh.local = new_local_ops;
            self.current_seq = seq;
            Ok(Some(op))
        }

    }
}

impl<O: Operation> ServerEngine<O> {

    fn new() -> ServerEngine<O> {
        ServerEngine {
            gh: GlobalHistory { global: vec![], local: vec![], },
            current_seq: 0,
        }
    }

    /// Process `op` from a client, return the transformed `ops`.
    fn process_ops(&mut self, mut ops: Vec<FatOp<O>>, context: GlobalSeq) -> EngineResult<Vec<FatOp<O>>, O> {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn make_fatop<O: Operation>(op: O, site: &SiteId) -> FatOp<O> {
        FatOp {
            seq: None,
            site: site.to_string(),
            site_seq: 1, // Dummy value.
            doc: "".to_string(),
            op,
        }
    }

    #[test]
    fn deopt_puzzle() {
        let mut doc_a = "abcd".to_string();
        let mut doc_b = "abcd".to_string();

        let site_a = "A".to_string();
        let site_b = "B".to_string();
        let mut client_a = ClientEngine::<SimpleOp>::new(&site_a);
        let mut client_b = ClientEngine::<SimpleOp>::new(&site_b);

        let mut server = ServerEngine::<SimpleOp>::new();

        let op_a1 = make_fatop(SimpleOp::Del(0, Some('a')), &site_a);
        let op_a2 = make_fatop(SimpleOp::Ins(2, 'x'), &site_a);
        let op_b1 = make_fatop(SimpleOp::Del(2, Some('c')), &site_b);

        // Local edits.
        apply(&mut doc_a, &op_a1.op);
        client_a.process_local_op(op_a1.clone());
        assert_eq!(doc_a, "bcd");

        apply(&mut doc_a, &op_a2.op);
        client_a.process_local_op(op_a2.clone());
        assert_eq!(doc_a, "bcxd");

        apply(&mut doc_b, &op_b1.op);
        client_b.process_local_op(op_b1.clone());
        assert_eq!(doc_b, "abd");

        // Server processing.
        let a_ops = server.process_ops(vec![op_a1, op_a2], 0).unwrap();
        let b_ops = server.process_ops(vec![op_b1], 0).unwrap();

        // Client A processing.
        let a1_at_a = client_a.process_remote_op(a_ops[0].clone()).unwrap();
        assert_eq!(a1_at_a, None);
        let a2_at_a = client_a.process_remote_op(a_ops[1].clone()).unwrap();
        assert_eq!(a2_at_a, None);
        let b1_at_a = client_a.process_remote_op(b_ops[0].clone()).unwrap().unwrap();

        apply(&mut doc_a, &b1_at_a.op);
        assert_eq!(doc_a, "bxd");

        // Client B processing.
        let a1_at_b = client_b.process_remote_op(a_ops[0].clone()).unwrap().unwrap();
        let a2_at_b = client_b.process_remote_op(a_ops[1].clone()).unwrap().unwrap();
        let b1_at_b = client_b.process_remote_op(b_ops[0].clone()).unwrap();
        assert_eq!(b1_at_b, None);

        apply(&mut doc_b, &a1_at_b.op);
        assert_eq!(doc_b, "bd");
        apply(&mut doc_b, &a2_at_b.op);
        assert_eq!(doc_b, "bxd");
    }

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

        let op_a1 = make_fatop(SimpleOp::Ins(2, '1'), &site_a);
        let op_b1 = make_fatop(SimpleOp::Ins(1, '2'), &site_b);
        let op_c1 = make_fatop(SimpleOp::Del(1, Some('b')), &site_c);

        // Local edits.
        apply(&mut doc_a, &op_a1.op);
        client_a.process_local_op(op_a1.clone());
        assert_eq!(doc_a, "ab1c");

        apply(&mut doc_b, &op_b1.op);
        client_b.process_local_op(op_b1.clone());
        assert_eq!(doc_b, "a2bc");

        apply(&mut doc_c, &op_c1.op);
        client_c.process_local_op(op_c1.clone());
        assert_eq!(doc_c, "ac");

        // Server processing.
        let b_ops = server.process_ops(vec![op_b1], 0).unwrap();
        let c_ops = server.process_ops(vec![op_c1], 0).unwrap();
        let a_ops = server.process_ops(vec![op_a1], 0).unwrap();

        // Client A processing.
        let b1_at_a = client_a.process_remote_op(b_ops[0].clone()).unwrap().unwrap();
        let c1_at_a = client_a.process_remote_op(c_ops[0].clone()).unwrap().unwrap();
        let a1_at_a = client_a.process_remote_op(a_ops[0].clone()).unwrap();

        apply(&mut doc_a, &b1_at_a.op);
        assert_eq!(doc_a, "a2b1c");
        apply(&mut doc_a, &c1_at_a.op);
        assert_eq!(doc_a, "a21c");

        // Client B processing.
        let b1_at_b = client_b.process_remote_op(b_ops[0].clone()).unwrap();
        let c1_at_b = client_b.process_remote_op(c_ops[0].clone()).unwrap().unwrap();
        let a1_at_b = client_b.process_remote_op(a_ops[0].clone()).unwrap().unwrap();

        apply(&mut doc_b, &c1_at_b.op);
        assert_eq!(doc_b, "a2c");
        apply(&mut doc_b, &a1_at_b.op);
        assert_eq!(doc_b, "a21c");

        // Client C processing.
        let b1_at_c = client_c.process_remote_op(b_ops[0].clone()).unwrap().unwrap();
        let c1_at_c = client_c.process_remote_op(c_ops[0].clone()).unwrap();
        let a1_at_c = client_c.process_remote_op(a_ops[0].clone()).unwrap().unwrap();

        apply(&mut doc_c, &b1_at_c.op);
        assert_eq!(doc_c, "a2c");
        apply(&mut doc_c, &a1_at_c.op);
        assert_eq!(doc_c, "a21c");
    }
}
