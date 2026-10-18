//! Ordered blocking work with asynchronous receipts. Once accepted, a job runs
//! to completion even if its caller is cancelled; a later barrier observes it.
use tokio::sync::{mpsc, oneshot};

type Job<S> = Box<dyn FnOnce(&mut S) + Send>;

pub(super) struct BlockingOwner<S> {
    sender: mpsc::Sender<Job<S>>,
}

impl<S> Clone for BlockingOwner<S> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<S: Send + 'static> BlockingOwner<S> {
    pub(super) fn new(name: &str, mut state: S) -> std::io::Result<Self> {
        let (sender, mut receiver) = mpsc::channel::<Job<S>>(64);
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                while let Some(job) = receiver.blocking_recv() {
                    job(&mut state);
                }
            })?;
        Ok(Self { sender })
    }

    pub(super) async fn run<R: Send + 'static>(
        &self,
        work: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> Result<R, String> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(Box::new(move |state| {
                let _ = tx.send(work(state));
            }))
            .await
            .map_err(|_| "storage worker stopped".to_string())?;
        rx.await
            .map_err(|_| "storage worker stopped before acknowledging its operation".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_waiters_do_not_cancel_accepted_writes_or_block_controls() {
        let owner = BlockingOwner::new("test-writer", Vec::<u8>::new()).unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let clone = owner.clone();
        let first = tokio::spawn(async move {
            clone
                .run(move |state| {
                    let _ = entered_tx.send(());
                    release_rx.recv().unwrap();
                    state.push(1);
                })
                .await
        });
        entered_rx.await.unwrap();
        // A single-thread runtime remains free while its writer is stalled.
        tokio::time::timeout(std::time::Duration::from_secs(1), tokio::task::yield_now())
            .await
            .unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        release_tx.send(()).unwrap();
        let state = owner
            .run(|state| {
                state.push(2);
                state.clone()
            })
            .await
            .unwrap();
        assert_eq!(state, [1, 2], "a later receipt is also a drain barrier");
    }
}
