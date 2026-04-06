//! Per-instance concurrency cap for `/v1/parse` and `/v1/extract`.
//!
//! The dispatcher path is CPU-bound (pdf_oxide, tesseract) and IO-bound
//! (PaddleOCR sidecar HTTP). Without a cap, a burst of large uploads can
//! exhaust tokio's blocking pool, RAM, or the Paddle queue — and there is
//! no mechanism to shed load gracefully.
//!
//! `ParseGate` wraps an `Arc<Semaphore>` and is registered as
//! `web::Data<ParseGate>` in `main.rs`. Both parse handlers attempt
//! `try_acquire` before doing any expensive work. On failure they return
//! `AppError::ServiceBusy` (`503 Service Unavailable` with
//! `Retry-After: 5`) immediately, rather than queueing on top of an
//! already-saturated worker pool.
//!
//! ## Why `try_acquire` and not `acquire`
//!
//! Queueing requests behind a saturated semaphore just trades a 503 for a
//! 504 once the request-level deadline fires. Failing fast also gives load
//! balancers a clear "drain me" signal so they can route to other instances.
//!
//! ## Cancellation note
//!
//! `tokio::time::timeout` can cancel the *future*, but the
//! `spawn_blocking` work it dispatched continues to completion. The gate
//! is what prevents abandoned-but-still-running tasks from compounding —
//! a permit stays held until the inner future completes (success, error,
//! or timeout drop), so the gate accurately reflects how much concurrent
//! work is *actually* in flight, not just how many futures are awaiting.

use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

/// Concurrency gate for parse + extract handlers.
#[derive(Clone, Debug)]
pub struct ParseGate {
    sem: Arc<Semaphore>,
    /// Configured capacity. Held for the metrics gauge planned in the
    /// observability tier; `#[allow(dead_code)]` until that lands.
    #[allow(dead_code)]
    capacity: usize,
}

impl ParseGate {
    /// Build a new gate with the given permit count. `capacity` must be
    /// `>= 1` — `AppConfig::from_vars` enforces this.
    pub fn new(capacity: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(capacity)),
            capacity,
        }
    }

    /// Try to acquire a permit without waiting. Returns the held permit on
    /// success; on failure the caller should map to
    /// `AppError::ServiceBusy`.
    pub fn try_acquire(&self) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        Arc::clone(&self.sem).try_acquire_owned()
    }

    /// Total permit count (the configured cap). Currently used only by
    /// the gate's own tests; will be exposed via the metrics gauge in
    /// Phase 3.1.
    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of permits currently in use. Currently used only by the
    /// gate's own tests; will back the `parse_gate_in_flight` metric.
    #[allow(dead_code)]
    pub fn in_flight(&self) -> usize {
        self.capacity - self.sem.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_gate_has_full_capacity() {
        let gate = ParseGate::new(4);
        assert_eq!(gate.capacity(), 4);
        assert_eq!(gate.in_flight(), 0);
    }

    #[test]
    fn try_acquire_succeeds_under_capacity() {
        let gate = ParseGate::new(2);
        let _p1 = gate.try_acquire().expect("first permit");
        let _p2 = gate.try_acquire().expect("second permit");
        assert_eq!(gate.in_flight(), 2);
    }

    #[test]
    fn try_acquire_fails_when_full() {
        let gate = ParseGate::new(1);
        let _held = gate.try_acquire().expect("first permit");
        assert!(
            gate.try_acquire().is_err(),
            "second acquire must fail when at capacity"
        );
    }

    #[test]
    fn permit_release_makes_capacity_available_again() {
        let gate = ParseGate::new(1);
        {
            let _held = gate.try_acquire().expect("first permit");
            assert_eq!(gate.in_flight(), 1);
        }
        assert_eq!(gate.in_flight(), 0);
        let _again = gate
            .try_acquire()
            .expect("must reacquire after first permit dropped");
    }

    #[test]
    fn gate_clone_shares_state() {
        let gate = ParseGate::new(1);
        let clone = gate.clone();
        let _held = gate.try_acquire().expect("first permit");
        assert!(
            clone.try_acquire().is_err(),
            "clone must observe parent's permit usage"
        );
    }
}
