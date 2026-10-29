#![forbid(unsafe_code)]

//! Bounded, executor-independent ownership handoff for many producers and one
//! logical receiver. Scheduling, saturation policy and payloads belong to callers.

use core::num::NonZeroUsize;

pub use tachyonix::{SendError, TryRecvError, TrySendError};

/// A cloneable producer. Acceptance does not imply delivery or processing.
pub struct Sender<T> {
    inner: tachyonix::Sender<T>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Sender<T> {
    /// Immediately accepts the value or returns ownership as Full/Closed.
    ///
    /// Does not wait for receiver progress. Use this on realtime paths.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.inner.try_send(value)
    }

    /// Waits for space on explicitly asynchronous control paths.
    ///
    /// Cancelling a pending send drops its unaccepted value, without enqueuing
    /// it. A closed queue returns ownership in SendError.
    pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
        self.inner.send(value).await
    }
}

/// The sole receive owner. May move between workers, but cannot be cloned or
/// driven concurrently. All receive operations require exclusive access.
pub struct Receiver<T> {
    inner: tachyonix::Receiver<T>,
}

impl<T> Receiver<T> {
    /// Returns the next value, Empty, or Closed after all producers are gone and
    /// accepted values have drained.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.inner.try_recv()
    }

    /// Appends at most max values to out, returning the number appended.
    ///
    /// max == 0 does nothing. Reserve output storage beforehand to avoid caller
    /// allocations. There is no internal batch or spillover buffer.
    pub fn try_recv_batch(&mut self, out: &mut Vec<T>, max: usize) -> usize {
        let mut received = 0;
        while received < max {
            let Ok(value) = self.try_recv() else {
                break;
            };
            out.push(value);
            received = received.saturating_add(1);
        }
        received
    }

    /// Waits for data without consuming it or freeing capacity.
    ///
    /// Safe to cancel and restart in select loops. Returns false only once
    /// closed and drained, and remains immediately true while data is pending.
    pub async fn readable(&mut self) -> bool {
        self.inner.readable().await
    }

    /// Receives the next value, or None once closed and drained.
    ///
    /// Cancelling a pending receive does not consume a value.
    pub async fn recv(&mut self) -> Option<T> {
        self.inner.recv().await.ok()
    }
}

/// Constructs a strictly bounded queue. No payload is copied or silently
/// dropped. Capacity includes all accepted, not-yet-received values.
///
/// Like the backend, capacities larger than usize::MAX / 2 + 1 panic.
pub fn bounded<T>(capacity: NonZeroUsize) -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = tachyonix::channel(capacity.get());
    (Sender { inner: sender }, Receiver { inner: receiver })
}

#[cfg(test)]
mod tests;
