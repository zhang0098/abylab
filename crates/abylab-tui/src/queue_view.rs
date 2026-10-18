//! Client view of durable queue rows. Owns optimistic identities, edit state,
//! steering handoffs and transcript-index remapping; backend receipts own delivery.
use crate::app::StagedBlock;
use std::collections::{HashMap, VecDeque};

pub(crate) struct QueuedPrompt {
    pub(crate) id: u64,
    pub(crate) blocks: Vec<StagedBlock>,
    pub(crate) cells: Vec<usize>,
}

/// A Send Now bubble awaiting settlement: the echo cells to re-tint (or to
/// hand back to the queue) plus the blocks themselves, so a deferred steer
/// requeues as the very prompt the user sent — images included.
pub(crate) struct PendingSteer {
    pub(crate) cells: Vec<usize>,
    pub(crate) blocks: Vec<StagedBlock>,
}

/// The queued prompt currently loaded into the composer for editing.
pub(crate) struct QueueEditState {
    pub(crate) prompt_id: u64,
    /// `ctrl+d` arms before it deletes: the first press asks, the second does.
    pub(crate) delete_confirm: bool,
}

pub(crate) struct QueueView {
    pub(crate) rows: VecDeque<QueuedPrompt>,
    pub(crate) edit: Option<QueueEditState>,
    pub(crate) steering: HashMap<u64, PendingSteer>,
    next_id: u64,
}
impl Default for QueueView {
    fn default() -> Self {
        Self {
            rows: VecDeque::new(),
            edit: None,
            steering: HashMap::new(),
            next_id: 1,
        }
    }
}
impl QueueView {
    pub(crate) fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
    pub(crate) fn enqueue(&mut self, id: u64, blocks: Vec<StagedBlock>, cells: Vec<usize>) {
        self.rows.push_back(QueuedPrompt { id, blocks, cells });
    }
    pub(crate) fn take(&mut self, id: u64) -> Option<QueuedPrompt> {
        self.rows
            .remove(self.rows.iter().position(|row| row.id == id)?)
    }
    /// A handoff ack is not a commit. Only rejected inputs return to the
    /// queue; accepted steers retain their cells until the admission receipt.
    pub(crate) fn rejected(&mut self, id: u64, deferred: bool) -> Option<PendingSteer> {
        deferred.then(|| self.steering.remove(&id)).flatten()
    }
    pub(crate) fn reset(&mut self) {
        self.rows.clear();
        self.edit = None;
        self.steering.clear();
    }
    pub(crate) fn remap(&mut self, shift: &crate::transcript::CellShift, skip: Option<usize>) {
        let remap = |cells: &mut Vec<usize>| {
            cells.retain_mut(|index| match shift.map(*index) {
                Some(at) => {
                    *index = at;
                    true
                }
                None => false,
            });
        };
        for (index, row) in self.rows.iter_mut().enumerate() {
            if Some(index) != skip {
                remap(&mut row.cells);
            }
        }
        for row in self.steering.values_mut() {
            remap(&mut row.cells);
        }
    }
}
