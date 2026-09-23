use crate::{CancellationToken, Error, ErrorKind, RequestPurpose, RequestRecord, Result, Usage};
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;

pub(crate) type Ledger = Arc<Mutex<Vec<RequestRecord>>>;

/// The stall clock an optional run window is measured by: the last moment the
/// run demonstrably moved.
///
/// deepseek-harness has no deadline over a whole step: it bounds the network
/// (per-request first-byte and stream-idle timeouts), each tool's own budget and
/// the request budget, and otherwise lets a step run as long as it keeps doing
/// something. abylab defaults to the same shape; [`crate::RunOptions::timeout`]
/// is available for a host that wants a run which stops moving to fail instead
/// of waiting forever, and a window set that way never cuts work that keeps
/// going.
#[derive(Debug)]
struct Progress {
    /// Last moment the run moved: bytes arrived, a request finished, a tool
    /// result was committed. Only this, never the segment's total duration,
    /// is what the window measures.
    last: Mutex<Instant>,
}

#[derive(Clone)]
pub(crate) struct RequestContext {
    pub cancellation: CancellationToken,
    /// How long the run may go without progress; `None` sets no such bound.
    pub window: Option<Duration>,
    progress: Arc<Progress>,
    count: Arc<AtomicUsize>,
    max_requests: usize,
    pub ledger: Ledger,
}

impl RequestContext {
    pub fn new(
        cancellation: CancellationToken,
        window: Option<Duration>,
        max_requests: usize,
        ledger: Ledger,
    ) -> Result<Self> {
        if let Some(window) = window {
            if window.is_zero() {
                return Err(Error::new(
                    ErrorKind::Configuration,
                    "timeout must be positive",
                ));
            }
            // A window this far out cannot be represented as a deadline at all;
            // it is not a limit, so reject it rather than silently treating it
            // as one.
            Instant::now()
                .checked_add(window)
                .ok_or_else(|| Error::new(ErrorKind::Configuration, "timeout is too large"))?;
        }
        Ok(Self {
            cancellation,
            window,
            progress: Arc::new(Progress {
                last: Mutex::new(Instant::now()),
            }),
            count: Arc::new(AtomicUsize::new(0)),
            max_requests,
            ledger,
        })
    }

    /// Note that the run is moving: a chunk arrived, a request or a tool call
    /// finished, work was committed. This is what keeps a long run alive.
    pub(crate) fn touch(&self) {
        let mut last = self.progress.last.lock().unwrap_or_else(|p| p.into_inner());
        *last = Instant::now();
    }

    /// The stall window itself, for callers pacing a retry: waiting longer than
    /// a whole window cannot be useful, because the run must move within one.
    pub(crate) fn window(&self) -> Option<Duration> {
        self.window
    }

    /// Whether the run has already gone quiet for a whole window. Callers that
    /// were about to wait again ask this instead: a stalled run is handed to the
    /// host, never retried behind its back. Always false without a window.
    pub(crate) fn stalled(&self) -> bool {
        self.window
            .is_some_and(|window| self.stalled_for() >= window)
    }

    fn stalled_for(&self) -> Duration {
        let last = *self.progress.last.lock().unwrap_or_else(|p| p.into_inner());
        Instant::now().saturating_duration_since(last)
    }

    pub fn check(&self) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(Error::new(ErrorKind::Cancelled, "operation cancelled"));
        }
        let Some(window) = self.window else {
            return Ok(());
        };
        let stalled = self.stalled_for();
        if stalled >= window {
            let gap = if stalled.as_secs() > 0 {
                format!("{}s", stalled.as_secs())
            } else {
                format!("{}ms", stalled.as_millis())
            };
            return Err(Error::new(
                ErrorKind::Timeout,
                format!("the run stalled: no progress for {gap}"),
            ));
        }
        Ok(())
    }

    /// Await host or provider work under cancellation, and under the stall
    /// window when the host set one.
    ///
    /// The window is a *gap* between two moments of progress, not a total
    /// duration: every wake-up re-reads it, so work that keeps moving re-arms
    /// the wait instead of being cut off at a fixed point.
    pub async fn wait<T>(&self, future: impl Future<Output = T>) -> Result<T> {
        let mut future = std::pin::pin!(future);
        loop {
            self.check()?;
            let Some(window) = self.window else {
                return tokio::select! {
                    biased;
                    _ = self.cancellation.cancelled() => Err(Error::new(ErrorKind::Cancelled, "operation cancelled")),
                    result = &mut future => Ok(result),
                };
            };
            let until = Instant::now() + window;
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => return Err(Error::new(ErrorKind::Cancelled, "operation cancelled")),
                result = &mut future => return Ok(result),
                // Not a failure: the wake-up only re-evaluates the gap.
                _ = tokio::time::sleep_until(until) => {}
            }
        }
    }

    /// Await one operation that owns a deadline of its own (a request under the
    /// transport's timeouts, a tool call under its budget). The run's window
    /// still applies when the host set one: an operation that produces nothing
    /// for a whole window is exactly what the window is looking for, whichever
    /// bound fires first.
    pub async fn timed<T>(&self, duration: Duration, future: impl Future<Output = T>) -> Result<T> {
        self.wait(tokio::time::timeout(duration, future))
            .await?
            .map_err(|_| Error::new(ErrorKind::Timeout, "network or tool timeout"))
    }

    pub fn reserve(
        &self,
        purpose: RequestPurpose,
        attempt: u32,
        request_bytes: Option<usize>,
    ) -> Result<usize> {
        self.check()?;
        self.count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < self.max_requests).then(|| n + 1)
            })
            .map_err(|_| Error::new(ErrorKind::BudgetExceeded, "HTTP request budget exhausted"))?;
        let mut ledger = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        let index = ledger.len();
        ledger.push(RequestRecord {
            purpose,
            attempt,
            response_id: None,
            status: None,
            usage: None,
            request_bytes,
        });
        Ok(index)
    }

    pub fn record(
        &self,
        index: usize,
        id: Option<String>,
        status: impl Into<String>,
        usage: Option<Usage>,
    ) {
        let mut ledger = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        ledger[index].response_id = id;
        ledger[index].status = Some(status.into());
        ledger[index].usage = usage;
    }
}

pub(crate) fn records(ledger: &Ledger) -> Vec<RequestRecord> {
    ledger.lock().unwrap_or_else(|p| p.into_inner()).clone()
}
