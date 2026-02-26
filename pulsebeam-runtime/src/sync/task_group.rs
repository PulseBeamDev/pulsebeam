use crate::sync::bit_signal::BitSignal;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

pub struct TaskGroup<T, O>
where
    T: Future<Output = O> + Send,
{
    signal: Arc<BitSignal>,
    /// Tracks which slots are currently filled (1 = Active, 0 = Empty).
    occupied: u64,
    /// Storage for long-lived participant tasks.
    slots: [Option<Pin<Box<T>>>; 64],
}

impl<T, O> TaskGroup<T, O>
where
    T: Future<Output = O> + Send,
{
    pub fn new(signal: Arc<BitSignal>) -> Self {
        Self {
            signal,
            occupied: 0,
            slots: std::array::from_fn(|_| None),
        }
    }

    /// Finds an empty slot and pins the task.
    /// Returns the index (0-63) used for BitSignal::notify.
    pub fn try_push(&mut self, task: T) -> Option<u8> {
        // Find the first 0 bit in the occupied mask.
        let index = (!self.occupied).trailing_zeros() as usize;

        if index < 64 {
            self.occupied |= 1 << index;
            self.slots[index] = Some(Box::pin(task));
            Some(index as u8)
        } else {
            None
        }
    }

    pub fn poll_group(&mut self, cx: &mut Context<'_>) -> Poll<(u8, O)> {
        self.signal.register(cx);
        let mut bits = self.signal.take();
        bits &= self.occupied;

        while bits != 0 {
            let i = bits.trailing_zeros() as usize;

            if let Some(task) = &mut self.slots[i]
                && let Poll::Ready(output) = task.as_mut().poll(cx)
            {
                // Task finished - cleanup and return.
                self.slots[i] = None;
                self.occupied &= !(1 << i);
                return Poll::Ready((i as u8, output));
            }

            // Clear the lowest set bit.
            bits &= bits.wrapping_sub(1);
        }

        Poll::Pending
    }
}

impl<T, O> Future for TaskGroup<T, O>
where
    T: Future<Output = O> + Send,
{
    type Output = (u8, O);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_group(cx)
    }
}
