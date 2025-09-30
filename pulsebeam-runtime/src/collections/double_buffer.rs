/// A double-buffered container optimized for batch operations with partial sends.
///
/// Uses two internal buffers to avoid expensive `drain` operations. Items are
/// accumulated in one buffer, then swapped for sending. Unsent items remain in
/// the send buffer for the next flush attempt.
#[derive(Debug)]
pub struct DoubleBuffer<T> {
    /// Buffer for accumulating new items
    accumulate: Vec<T>,
    /// Buffer for items being sent (may contain unsent items from previous flush)
    send: Vec<T>,
}

impl<T> DoubleBuffer<T> {
    /// Creates a new double buffer with the given capacity for each buffer
    pub fn new(capacity: usize) -> Self {
        Self {
            accumulate: Vec::with_capacity(capacity),
            send: Vec::with_capacity(capacity),
        }
    }

    /// Adds an item to the accumulation buffer
    #[inline]
    pub fn push(&mut self, item: T) {
        self.accumulate.push(item);
    }

    /// Returns true if there are no items pending (in either buffer)
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.accumulate.is_empty() && self.send.is_empty()
    }

    /// Returns the total number of items pending across both buffers
    #[inline]
    pub fn len(&self) -> usize {
        self.accumulate.len() + self.send.len()
    }

    /// Prepares items for sending and returns a slice to send.
    ///
    /// This method:
    /// 1. Returns unsent items from previous flush if any exist
    /// 2. Otherwise swaps buffers and returns newly accumulated items
    ///
    /// After getting the slice, call `mark_sent()` with the number of items sent.
    ///
    /// Returns None if there are no items to send.
    pub fn prepare_send(&mut self) -> Option<&[T]> {
        // If we have unsent items from previous flush, return those first
        if !self.send.is_empty() {
            return Some(&self.send);
        }

        // Nothing to send
        if self.accumulate.is_empty() {
            return None;
        }

        // Swap buffers (O(1))
        std::mem::swap(&mut self.accumulate, &mut self.send);
        Some(&self.send)
    }

    /// Marks the given number of items as sent.
    ///
    /// This should be called after attempting to send the slice from `prepare_send()`.
    /// Items not marked as sent will be retried on the next `prepare_send()` call.
    ///
    /// # Panics
    /// Panics if `count` is greater than the number of items in the send buffer.
    pub fn mark_sent(&mut self, count: usize) {
        assert!(
            count <= self.send.len(),
            "Cannot mark {} items as sent when only {} items are in send buffer",
            count,
            self.send.len()
        );

        if count == 0 {
            return;
        }

        if count == self.send.len() {
            // Fast path: all sent
            self.send.clear();
        } else {
            // Partial send: drain sent items
            self.send.drain(..count);
        }
    }

    /// Returns the number of items in the accumulation buffer
    #[inline]
    pub fn accumulate_len(&self) -> usize {
        self.accumulate.len()
    }

    /// Returns the number of items in the send buffer (unsent from previous flush)
    #[inline]
    pub fn send_len(&self) -> usize {
        self.send.len()
    }

    /// Clears all items from both buffers
    pub fn clear(&mut self) {
        self.accumulate.clear();
        self.send.clear();
    }
}

impl<T> Default for DoubleBuffer<T> {
    fn default() -> Self {
        Self::new(64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_buffer() {
        let mut buf = DoubleBuffer::<i32>::new(10);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.accumulate_len(), 0);
        assert_eq!(buf.send_len(), 0);
        assert!(buf.prepare_send().is_none());
    }

    #[test]
    fn test_push_and_length() {
        let mut buf = DoubleBuffer::new(10);

        buf.push(1);
        buf.push(2);
        buf.push(3);

        assert!(!buf.is_empty());
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.accumulate_len(), 3);
        assert_eq!(buf.send_len(), 0);
    }

    #[test]
    fn test_full_send_success() {
        let mut buf = DoubleBuffer::new(10);
        buf.push(1);
        buf.push(2);
        buf.push(3);

        // Prepare and send all items
        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[1, 2, 3]);
        assert_eq!(buf.accumulate_len(), 0); // Swapped to send
        assert_eq!(buf.send_len(), 3);

        buf.mark_sent(3);

        assert!(buf.is_empty());
        assert_eq!(buf.accumulate_len(), 0);
        assert_eq!(buf.send_len(), 0);
    }

    #[test]
    fn test_partial_send() {
        let mut buf = DoubleBuffer::new(10);
        buf.push(1);
        buf.push(2);
        buf.push(3);
        buf.push(4);
        buf.push(5);

        // Prepare items
        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[1, 2, 3, 4, 5]);

        // Only send first 2 items
        buf.mark_sent(2);

        assert!(!buf.is_empty());
        assert_eq!(buf.len(), 3); // 3 items remain
        assert_eq!(buf.accumulate_len(), 0);
        assert_eq!(buf.send_len(), 3); // 3 unsent in send buffer
    }

    #[test]
    fn test_retry_after_partial_send() {
        let mut buf = DoubleBuffer::new(10);
        buf.push(1);
        buf.push(2);
        buf.push(3);

        // First send: partial
        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[1, 2, 3]);
        buf.mark_sent(1); // Only sent first item

        assert_eq!(buf.len(), 2);

        // Second send: get remaining items
        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[2, 3]); // Unsent items from before
        buf.mark_sent(2);

        assert!(buf.is_empty());
    }

    #[test]
    fn test_accumulate_during_blocked_send() {
        let mut buf = DoubleBuffer::new(10);
        buf.push(1);
        buf.push(2);
        buf.push(3);

        // First attempt: nothing sent (socket blocked)
        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[1, 2, 3]);
        buf.mark_sent(0); // Nothing sent

        assert_eq!(buf.send_len(), 3); // All in send buffer

        // Add more items while blocked
        buf.push(4);
        buf.push(5);

        assert_eq!(buf.len(), 5);
        assert_eq!(buf.accumulate_len(), 2); // New items
        assert_eq!(buf.send_len(), 3); // Old items still blocked

        // Second attempt: sends old items first
        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[1, 2, 3]); // Still the old items
        buf.mark_sent(3);

        // Third attempt: sends new items
        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[4, 5]);
        buf.mark_sent(2);

        assert!(buf.is_empty());
    }

    #[test]
    fn test_multiple_partial_sends() {
        let mut buf = DoubleBuffer::new(10);
        for i in 1..=10 {
            buf.push(i);
        }

        // Send 1: send 3 items
        let items = buf.prepare_send().unwrap();
        assert_eq!(&items[..3], &[1, 2, 3]);
        buf.mark_sent(3);
        assert_eq!(buf.len(), 7);

        // Send 2: send 2 more
        let items = buf.prepare_send().unwrap();
        assert_eq!(&items[..2], &[4, 5]);
        buf.mark_sent(2);
        assert_eq!(buf.len(), 5);

        // Send 3: send remaining
        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[6, 7, 8, 9, 10]);
        buf.mark_sent(5);

        assert!(buf.is_empty());
    }

    #[test]
    fn test_prepare_send_empty_buffer() {
        let mut buf = DoubleBuffer::<i32>::new(10);
        assert!(buf.prepare_send().is_none());
    }

    #[test]
    fn test_mark_sent_zero() {
        let mut buf = DoubleBuffer::new(10);
        buf.push(1);
        buf.push(2);

        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[1, 2]);

        buf.mark_sent(0); // No-op

        assert_eq!(buf.send_len(), 2);
        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[1, 2]); // Still there
    }

    #[test]
    #[should_panic(expected = "Cannot mark 5 items as sent when only 3 items are in send buffer")]
    fn test_mark_sent_too_many() {
        let mut buf = DoubleBuffer::new(10);
        buf.push(1);
        buf.push(2);
        buf.push(3);

        buf.prepare_send();
        buf.mark_sent(5); // Panic: more than available
    }

    #[test]
    fn test_clear() {
        let mut buf = DoubleBuffer::new(10);
        buf.push(1);
        buf.push(2);

        buf.prepare_send();
        buf.mark_sent(1);

        buf.push(3);

        assert!(!buf.is_empty());

        buf.clear();

        assert!(buf.is_empty());
        assert_eq!(buf.accumulate_len(), 0);
        assert_eq!(buf.send_len(), 0);
    }

    #[test]
    fn test_interleaved_push_and_send() {
        let mut buf = DoubleBuffer::new(10);

        // Round 1
        buf.push(1);
        buf.push(2);
        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[1, 2]);
        buf.mark_sent(2);

        // Round 2
        buf.push(3);
        buf.push(4);
        buf.push(5);
        let items = buf.prepare_send().unwrap();
        assert_eq!(items, &[3, 4, 5]);
        buf.mark_sent(3);

        assert!(buf.is_empty());
    }

    #[test]
    fn test_state_transitions() {
        let mut buf = DoubleBuffer::new(10);

        // State: Empty
        assert!(buf.is_empty());
        assert_eq!(buf.accumulate_len(), 0);
        assert_eq!(buf.send_len(), 0);

        // State: Accumulating
        buf.push(1);
        buf.push(2);
        assert_eq!(buf.accumulate_len(), 2);
        assert_eq!(buf.send_len(), 0);

        // State: Ready to send
        buf.prepare_send();
        assert_eq!(buf.accumulate_len(), 0);
        assert_eq!(buf.send_len(), 2);

        // State: Partial send
        buf.mark_sent(1);
        assert_eq!(buf.accumulate_len(), 0);
        assert_eq!(buf.send_len(), 1);

        // State: Accumulating while blocked
        buf.push(3);
        assert_eq!(buf.accumulate_len(), 1);
        assert_eq!(buf.send_len(), 1);

        // State: Finish old send
        buf.prepare_send(); // Returns old item
        buf.mark_sent(1);
        assert_eq!(buf.accumulate_len(), 1);
        assert_eq!(buf.send_len(), 0);

        // State: Send new items
        buf.prepare_send();
        buf.mark_sent(1);
        assert!(buf.is_empty());
    }
}
