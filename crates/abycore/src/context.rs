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

#[derive(Clone)]
pub(crate) struct RequestContext {
    pub cancellation: CancellationToken,
    pub deadline: Instant,
    count: Arc<AtomicUsize>,
    max_requests: usize,
    pub ledger: Ledger,
}

impl RequestContext {
    pub fn new(
        cancellation: CancellationToken,
        timeout: Duration,
        max_requests: usize,
        ledger: Ledger,
    ) -> Result<Self> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Error::new(ErrorKind::Configuration, "timeout is too large"))?;
        if timeout.is_zero() {
            return Err(Error::new(
                ErrorKind::Configuration,
                "timeout must be positive",
            ));
        }
        Ok(Self {
            cancellation,
            deadline,
            count: Arc::new(AtomicUsize::new(0)),
            max_requests,
            ledger,
        })
    }

    pub fn check(&self) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(Error::new(ErrorKind::Cancelled, "operation cancelled"));
        }
        if Instant::now() >= self.deadline {
            return Err(Error::new(
                ErrorKind::Timeout,
                "operation deadline exceeded",
            ));
        }
        Ok(())
    }

    pub async fn wait<T>(&self, future: impl Future<Output = T>) -> Result<T> {
        self.check()?;
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(Error::new(ErrorKind::Cancelled, "operation cancelled")),
            _ = tokio::time::sleep_until(self.deadline) => Err(Error::new(ErrorKind::Timeout, "operation deadline exceeded")),
            result = future => Ok(result),
        }
    }

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
