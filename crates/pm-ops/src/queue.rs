//! The list of pending operations the user builds before pressing "Apply".

use crate::model::Model;
use crate::operation::Operation;
use crate::planner::Planner;
use pm_core::Result;

/// Pending operations over a base [`Model`], with the preview of the disks as they will be
/// once every operation is applied.
#[derive(Clone, Debug)]
pub struct OpQueue {
    base: Model,
    ops: Vec<Operation>,
    preview: Model,
}

impl OpQueue {
    pub fn new(base: Model) -> Self {
        OpQueue { preview: base.clone(), base, ops: Vec::new() }
    }

    /// Validate `op` on the current preview ([`Planner::simulate`]) and append it. A rejected
    /// operation leaves the queue unchanged.
    pub fn push(&mut self, op: Operation) -> Result<()> {
        self.preview = Planner::simulate(&self.preview, &op)?;
        self.ops.push(op);
        Ok(())
    }

    /// Remove the last operation and recompute the preview from the base. Should an earlier
    /// operation no longer validate (e.g. an image file it reads has disappeared), the queue
    /// is cut back to the operations that still do, so the preview always matches `ops`.
    pub fn undo(&mut self) -> Option<Operation> {
        let op = self.ops.pop()?;
        self.recompute();
        Some(op)
    }

    /// Drop every pending operation.
    pub fn discard(&mut self) {
        self.ops.clear();
        self.preview = self.base.clone();
    }

    pub fn preview(&self) -> &Model {
        &self.preview
    }

    pub fn base(&self) -> &Model {
        &self.base
    }

    pub fn ops(&self) -> &[Operation] {
        &self.ops
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Start over from a new base (after Apply or a refresh): pending operations are dropped.
    pub fn reset(&mut self, base: Model) {
        self.base = base;
        self.discard();
    }

    fn recompute(&mut self) {
        let mut model = self.base.clone();
        let mut valid = 0;
        for op in &self.ops {
            match Planner::simulate(&model, op) {
                Ok(m) => {
                    model = m;
                    valid += 1;
                }
                Err(_) => break,
            }
        }
        self.ops.truncate(valid);
        self.preview = model;
    }
}
