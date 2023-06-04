use std::result::Result;
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
        let mut result = vec![];
        if seq < len {
            let start = (seq as usize) - 1;
            result.clone_from_slice(&self.global[start..]);
        }
        result
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
            Ok(None)
        } else {
            // We received an op generated at another site, transform
            // it, add it to history, and return it.
            let new_local_ops = op.symmetric_transform(&self.gh.local[..]);
            self.gh.local = new_local_ops;
            Ok(Some(op))
        }
    }
}

impl<O: Operation> ServerEngine<O> {
    /// Process `op` from a client, return the transformed `ops`.
    fn process_ops(&mut self, mut ops: Vec<FatOp<O>>, context: GlobalSeq) -> EngineResult<Vec<FatOp<O>>, O> {
        let l1 = self.gh.ops_after(context);
        for mut l1_op in l1 {
            ops = l1_op.symmetric_transform(&ops);
        }
        for mut op in &mut ops {
            op.seq = Some(self.current_seq + 1);
            self.current_seq += 1;
        }
        self.gh.global.clone_from_slice(&ops[..]);
        Ok(ops)
    }
}
