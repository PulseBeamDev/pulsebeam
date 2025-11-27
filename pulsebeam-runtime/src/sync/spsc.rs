use crossbeam_utils::CachePadded;
use futures::task::AtomicWaker;
use parking_lot::RwLock;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};

/// A single slot in the ring buffer.
/// We use RwLock to satisfy Rust's safety guarantees (interior mutability).
/// Since this is SPSC, the Producer and Consumer almost never touch the same slot
/// at the same time, so this lock is virtually free (no contention).
struct Slot<T> {
    item: RwLock<Option<T>>,
}

struct Shared<T> {
    /// The ring buffer of locks.
    buffer: Vec<Slot<T>>,
    /// Capacity mask (size - 1).
    mask: usize,
    /// Closed flag.
    closed: AtomicBool,

    /// --- Cache Line 1 (Producer Write / Consumer Read) ---
    head: CachePadded<AtomicUsize>,
    recv_waker: AtomicWaker,

    /// --- Cache Line 2 (Consumer Write / Producer Read) ---
    tail: CachePadded<AtomicUsize>,
    send_waker: AtomicWaker,
}

pub struct Producer<T> {
    shared: Arc<Shared<T>>,
    /// Local cached tail to avoid checking the atomic constantly.
    tail_shadow: usize,
    /// Local head for fast math.
    head_local: usize,
    _marker: PhantomData<T>,
}

pub struct Consumer<T> {
    shared: Arc<Shared<T>>,
    /// Local cached head to avoid checking the atomic constantly.
    head_shadow: usize,
    /// Local tail for fast math.
    tail_local: usize,
    _marker: PhantomData<T>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TrySendError<T> {
    Full(T),
    Closed(T),
}

// Implement Unpin so we can use them in poll_fn easily
impl<T> Unpin for Producer<T> {}
impl<T> Unpin for Consumer<T> {}

impl<T> Producer<T> {
    /// Non-blocking send.
    /// Returns `Err(Full(val))` if no space, allowing you to drop or retry.
    pub fn try_send(&mut self, val: T) -> Result<(), TrySendError<T>> {
        if self.shared.closed.load(Ordering::Relaxed) {
            return Err(TrySendError::Closed(val));
        }

        let mask = self.shared.mask;
        let head = self.head_local;
        let next_head = head.wrapping_add(1);

        // 1. Check Capacity (Shadow Index Optimization)
        // We assume we have space based on our local cache of 'tail'.
        if next_head.wrapping_sub(self.tail_shadow) > mask {
            // Shadow says full, check the actual atomic tail.
            let actual_tail = self.shared.tail.load(Ordering::Acquire);
            self.tail_shadow = actual_tail;

            if next_head.wrapping_sub(actual_tail) > mask {
                return Err(TrySendError::Full(val));
            }
        }

        // 2. Write Data
        // We know the slot is free, so acquiring this lock is uncontended and fast.
        {
            let slot = &self.shared.buffer[head & mask];
            *slot.item.write() = Some(val);
        }

        // 3. Publish Head
        // Release ordering ensures Consumer sees the data before seeing the new head index.
        self.shared.head.store(next_head, Ordering::Release);
        self.head_local = next_head;

        // 4. Wake Consumer
        self.shared.recv_waker.wake();

        Ok(())
    }

    /// Async send hook for integration with generic Sink traits.
    pub fn poll_send(&mut self, cx: &mut Context<'_>, val: T) -> Poll<Result<(), T>> {
        match self.try_send(val) {
            Ok(_) => Poll::Ready(Ok(())),
            Err(TrySendError::Closed(v)) => Poll::Ready(Err(v)),
            Err(TrySendError::Full(v)) => {
                self.shared.send_waker.register(cx.waker());
                // Double check to prevent race condition
                let actual_tail = self.shared.tail.load(Ordering::Acquire);
                self.tail_shadow = actual_tail;
                let next_head = self.head_local.wrapping_add(1);

                if next_head.wrapping_sub(actual_tail) <= self.shared.mask {
                    cx.waker().wake_by_ref();
                }
                Poll::Pending
            }
        }
    }
}

impl<T> Consumer<T> {
    /// Non-blocking receive.
    pub fn try_recv(&mut self) -> Option<T> {
        let mask = self.shared.mask;
        let tail = self.tail_local;

        // 1. Check Empty (Shadow Index Optimization)
        if tail == self.head_shadow {
            let actual_head = self.shared.head.load(Ordering::Acquire);
            self.head_shadow = actual_head;

            if tail == actual_head {
                return None;
            }
        }

        // 2. Read Data
        // We own this slot for reading.
        let val = {
            let slot = &self.shared.buffer[tail & mask];
            slot.item.write().take()
        };

        // 3. Publish Tail
        let next_tail = tail.wrapping_add(1);
        self.shared.tail.store(next_tail, Ordering::Release);
        self.tail_local = next_tail;

        // 4. Wake Producer
        self.shared.send_waker.wake();

        val
    }

    /// Async receive.
    pub async fn recv(&mut self) -> Option<T> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if let Some(val) = self.try_recv() {
            return Poll::Ready(Some(val));
        }

        // Check closed state
        if self.shared.closed.load(Ordering::Relaxed) {
            let actual_head = self.shared.head.load(Ordering::Acquire);
            if self.tail_local == actual_head {
                return Poll::Ready(None);
            }
        }

        self.shared.recv_waker.register(cx.waker());

        // Re-check after register
        if let Some(val) = self.try_recv() {
            return Poll::Ready(Some(val));
        }

        if self.shared.closed.load(Ordering::Relaxed) {
            let actual_head = self.shared.head.load(Ordering::Acquire);
            if self.tail_local == actual_head {
                return Poll::Ready(None);
            }
        }

        Poll::Pending
    }
}

impl<T> Drop for Producer<T> {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.recv_waker.wake();
    }
}

impl<T> Drop for Consumer<T> {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.send_waker.wake();
        // Safe Drop: Vec<RwLock> handles dropping contained items automatically.
    }
}

/// Creates a safe, optimized SPSC channel.
pub fn channel<T: Send>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    assert!(capacity > 0 && capacity.is_power_of_two());

    let mut buffer = Vec::with_capacity(capacity);
    for _ in 0..capacity {
        buffer.push(Slot {
            item: RwLock::new(None),
        });
    }

    let shared = Arc::new(Shared {
        buffer,
        mask: capacity - 1,
        closed: AtomicBool::new(false),
        head: CachePadded::new(AtomicUsize::new(0)),
        recv_waker: AtomicWaker::new(),
        tail: CachePadded::new(AtomicUsize::new(0)),
        send_waker: AtomicWaker::new(),
    });

    (
        Producer {
            shared: shared.clone(),
            tail_shadow: 0,
            head_local: 0,
            _marker: PhantomData,
        },
        Consumer {
            shared,
            head_shadow: 0,
            tail_local: 0,
            _marker: PhantomData,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::task;

    #[tokio::test]
    async fn test_send_trait() {
        let (mut p, mut c) = channel::<u32>(4);

        let h1 = task::spawn(async move {
            p.try_send(100).unwrap();
        });

        let h2 = task::spawn(async move { c.recv().await });

        h1.await.unwrap();
        let val = h2.await.unwrap();
        assert_eq!(val, Some(100));
    }

    #[tokio::test]
    async fn test_basic_send_recv() {
        let (mut p, mut c) = channel::<u32>(4);
        assert!(p.try_send(1).is_ok());
        assert!(p.try_send(2).is_ok());
        assert_eq!(c.try_recv(), Some(1));
        assert_eq!(c.try_recv(), Some(2));
    }

    #[tokio::test]
    async fn test_full_behavior() {
        let (mut p, _s) = channel::<u32>(2);
        p.try_send(1).unwrap();
        match p.try_send(3) {
            Err(TrySendError::Full(v)) => assert_eq!(v, 3),
            _ => panic!("Expected full"),
        }
    }

    #[tokio::test]
    async fn test_async_wake() {
        let (mut p, mut c) = channel::<u32>(4);
        let h = task::spawn(async move { c.recv().await });

        tokio::time::sleep(Duration::from_millis(5)).await;
        p.try_send(99).unwrap();

        assert_eq!(h.await.unwrap(), Some(99));
    }

    #[tokio::test]
    async fn test_close() {
        let (mut p, mut c) = channel::<u32>(4);
        p.try_send(1).unwrap();
        drop(p);
        assert_eq!(c.recv().await, Some(1));
        assert_eq!(c.recv().await, None);
    }
}
