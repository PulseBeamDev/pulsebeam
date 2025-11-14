use tracing::warn;

#[derive(Debug)]
pub struct RingBuffer<T: Clone> {
    buffer: Vec<Option<T>>,
    head: u64, // Next expected sequence number
    tail: u64, // Oldest sequence number in buffer
    initialized: bool,
}

impl<T: Clone> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: vec![None; capacity],
            head: 0,
            tail: 0,
            initialized: false,
        }
    }

    pub fn capacity(&self) -> u64 {
        self.buffer.len() as u64
    }

    pub fn head(&self) -> u64 {
        self.head
    }

    pub fn tail(&self) -> u64 {
        self.tail
    }

    fn initialize(&mut self, seq: u64) {
        self.head = seq.wrapping_add(1);
        self.tail = seq;
        self.initialized = true;
    }

    /// Checks if a sequence number is old and should be ignored.
    pub fn is_old(&self, seq: u64) -> bool {
        let tail_val = self.tail;
        let seq_val = seq;
        // If seq is more than half the u64 space behind tail, it's considered old.
        // This is a robust way to handle wrap-around.
        seq_val.wrapping_sub(tail_val) > (u64::MAX / 2)
    }

    /// Inserts an item at its sequence number.
    /// Returns true if the buffer had to be slid forward to make space.
    pub fn insert(&mut self, seq: u64, item: T) -> bool {
        if !self.initialized {
            self.initialize(seq);
        }

        if self.is_old(seq) {
            warn!(
                "Attempted to insert packet with seq {} which is older than tail {}, ignoring.",
                seq, self.tail
            );
            return false;
        }

        // Advance head if this packet is newer than what we've seen
        let head_val = self.head;
        let seq_val = seq;
        if seq_val.wrapping_sub(head_val) < (u64::MAX / 2) || seq == self.head {
            self.head = seq.wrapping_add(1);
        }

        // Check if we need to slide the tail forward to make space
        let offset_from_tail = seq_val.wrapping_sub(self.tail);
        let mut slid = false;
        if offset_from_tail >= self.capacity() {
            // New tail is calculated to keep the buffer size constant relative to the new head
            self.tail = self.head.wrapping_sub(self.capacity());
            slid = true;
        }

        let index = (seq % self.capacity()) as usize;
        self.buffer[index] = Some(item);

        slid
    }

    /// Takes the item at the current tail, advancing the tail if successful.
    pub fn pop_front(&mut self) -> Option<T> {
        if self.tail == self.head {
            return None; // Buffer is empty
        }

        let index = (self.tail % self.capacity()) as usize;
        let item = self.buffer[index].take();

        if item.is_some() {
            self.tail = self.tail.wrapping_add(1);
        }

        item
    }

    /// Advances the tail to a specific sequence number, returning the number of missed packets.
    pub fn advance_tail_to(&mut self, new_tail: u64) -> u64 {
        let old_tail_val = self.tail;
        let new_tail_val = new_tail;

        let distance = new_tail_val.wrapping_sub(old_tail_val);
        if distance == 0 || distance > u64::MAX / 2 {
            return 0; // No advancement or invalid advancement
        }

        let mut missed_packets = 0;
        let capacity = self.capacity();
        let iterations = distance.min(capacity);
        let start_seq = new_tail_val.wrapping_sub(iterations);

        for i in 0..iterations {
            let current_seq = start_seq.wrapping_add(i);
            let index = (current_seq % capacity) as usize;
            if self.buffer[index].take().is_none() {
                missed_packets += 1;
            }
        }

        // Account for packets that were too old to even be in the buffer window
        if distance > capacity {
            missed_packets += distance - capacity;
        }

        self.tail = new_tail;
        missed_packets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_and_capacity() {
        let buffer = RingBuffer::<i32>::new(10);
        assert_eq!(buffer.capacity(), 10);
        assert_eq!(buffer.head(), 0);
        assert_eq!(buffer.tail(), 0);
    }

    #[test]
    fn test_initialize() {
        let mut buffer = RingBuffer::<i32>::new(10);
        buffer.initialize(100);
        assert_eq!(buffer.tail(), 100);
        assert_eq!(buffer.head(), 101);
    }

    #[test]
    fn test_insert_in_order() {
        let mut buffer = RingBuffer::<i32>::new(10);
        buffer.initialize(0);
        assert!(!buffer.insert(0, 100));
        assert_eq!(buffer.tail(), 0);
        assert_eq!(buffer.head(), 1);

        assert!(!buffer.insert(1, 101));
        assert_eq!(buffer.tail(), 0);
        assert_eq!(buffer.head(), 2);
    }

    #[test]
    fn test_insert_out_of_order() {
        let mut buffer = RingBuffer::<i32>::new(10);
        buffer.initialize(0);

        // Insert a future packet
        assert!(!buffer.insert(5, 105));
        assert_eq!(buffer.tail(), 0);
        assert_eq!(buffer.head(), 6);

        // Insert a past packet
        assert!(!buffer.insert(1, 101));
        assert_eq!(buffer.tail(), 0);
        assert_eq!(buffer.head(), 6); // Head should not move backwards
    }

    #[test]
    fn test_pop_front() {
        let mut buffer = RingBuffer::new(10);
        buffer.initialize(50); // tail=50, head=51

        buffer.insert(50, 100);
        buffer.insert(51, 101);
        buffer.insert(53, 103); // Leave a gap at 52

        assert_eq!(buffer.tail(), 50);
        assert_eq!(buffer.head(), 54);

        // Pop the first item
        assert_eq!(buffer.pop_front(), Some(100));
        assert_eq!(buffer.tail(), 51);

        // Pop the second item
        assert_eq!(buffer.pop_front(), Some(101));
        assert_eq!(buffer.tail(), 52);

        // Try to pop the gap, tail should not advance
        assert_eq!(buffer.pop_front(), None);
        assert_eq!(buffer.tail(), 52);

        // Advance tail manually past the gap
        buffer.tail = 53;

        // Pop the third item
        assert_eq!(buffer.pop_front(), Some(103));
        assert_eq!(buffer.tail(), 54);

        // Buffer is now empty
        assert_eq!(buffer.pop_front(), None);
        assert_eq!(buffer.tail(), 54);
        assert_eq!(buffer.head(), 54);
    }

    #[test]
    fn test_pop_from_empty_buffer() {
        let mut buffer = RingBuffer::<i32>::new(5);
        assert_eq!(buffer.pop_front(), None);
        buffer.initialize(10);
        assert_eq!(buffer.pop_front(), None);
    }

    #[test]
    fn test_insert_old_packet() {
        let mut buffer = RingBuffer::new(10);
        buffer.initialize(100); // tail = 100, head = 101

        // Attempt to insert an old packet
        assert!(!buffer.insert(90, 999));

        // State should not change
        assert_eq!(buffer.tail(), 100);
        assert_eq!(buffer.head(), 101);
    }

    #[test]
    fn test_buffer_sliding() {
        let mut buffer = RingBuffer::<i32>::new(5);
        buffer.initialize(0); // tail=0, head=1, capacity=5

        // Fill the buffer
        for i in 0..5 {
            buffer.insert(i, (i * 10) as i32);
        }

        assert_eq!(buffer.tail(), 0);
        assert_eq!(buffer.head(), 5);

        // Insert an item that is far ahead, causing the window to slide
        let slid = buffer.insert(8, 80);
        assert!(slid);

        // head becomes 9. tail becomes head - capacity = 9 - 5 = 4
        assert_eq!(buffer.head(), 9);
        assert_eq!(buffer.tail(), 4);

        // The old value at index (4 % 5) should still be there until popped or overwritten
        // The new value is at index (8 % 5) = 3
        assert_eq!(buffer.buffer[3], Some(80));
        assert_eq!(buffer.buffer[4], Some(40));

        // Popping at the new tail should succeed
        assert_eq!(buffer.pop_front(), Some(40));
        assert_eq!(buffer.tail(), 5);
    }

    #[test]
    fn test_advance_tail_to() {
        let mut buffer = RingBuffer::new(10);

        // Insert some items with gaps
        buffer.insert(10, 100);
        buffer.insert(12, 120);
        buffer.insert(14, 140);
        // Current state: tail=10, head=15, buffer has items at seq 10, 12, 14

        // Advance past one present and one absent item
        let missed = buffer.advance_tail_to(12);
        assert_eq!(missed, 1); // Packet at seq 11 was missed
        assert_eq!(buffer.tail(), 12);

        // After advancing, the item at seq 10 should be gone (taken)
        assert_eq!(buffer.buffer[(10 % 10) as usize], None);

        // Advance with no missed packets
        let missed_2 = buffer.advance_tail_to(13);
        assert_eq!(missed_2, 0);
        assert_eq!(buffer.tail(), 13);

        // Advancing to the same tail should do nothing
        let missed_3 = buffer.advance_tail_to(13);
        assert_eq!(missed_3, 0);
        assert_eq!(buffer.tail(), 13);
    }

    #[test]
    fn test_advance_tail_to_large_jump() {
        let mut buffer = RingBuffer::<i32>::new(10);
        buffer.initialize(0);
        buffer.insert(1, 10);
        buffer.insert(5, 50);
        // tail=0, head=6

        // Jump far forward, past the capacity
        let missed = buffer.advance_tail_to(20);

        // Expected missed:
        // Within buffer window (from 20-10=10 to 20): all 10 are missed as nothing was inserted there.
        // Outside buffer window (from 0 to 10):
        // We had items at 1 and 5. So we missed 0, 2, 3, 4, 6, 7, 8, 9 (8 packets)
        // Total missed = (20 - 0) - 2 items = 18
        assert_eq!(missed, 18);
        assert_eq!(buffer.tail(), 20);

        // Check that the items are cleared from the buffer
        assert_eq!(buffer.buffer[1], None);
        assert_eq!(buffer.buffer[5], None);
    }

    #[test]
    fn test_wrapping_behavior() {
        let mut buffer = RingBuffer::new(10);
        let start_seq = u64::MAX - 3;
        buffer.initialize(start_seq); // tail = u64::MAX - 3, head = u64::MAX - 2

        buffer.insert(u64::MAX - 3, 1);
        buffer.insert(u64::MAX - 2, 2);
        buffer.insert(u64::MAX - 1, 3);
        buffer.insert(u64::MAX, 4);
        buffer.insert(0, 5); // Wrapped around
        buffer.insert(1, 6);

        assert_eq!(buffer.head(), 2);
        assert_eq!(buffer.tail(), u64::MAX - 3);

        assert_eq!(buffer.pop_front(), Some(1));
        assert_eq!(buffer.tail(), u64::MAX - 2);

        assert_eq!(buffer.pop_front(), Some(2));
        assert_eq!(buffer.tail(), u64::MAX - 1);

        assert_eq!(buffer.pop_front(), Some(3));
        assert_eq!(buffer.tail(), u64::MAX);

        assert_eq!(buffer.pop_front(), Some(4));
        assert_eq!(buffer.tail(), 0);

        assert_eq!(buffer.pop_front(), Some(5));
        assert_eq!(buffer.tail(), 1);

        assert_eq!(buffer.pop_front(), Some(6));
        assert_eq!(buffer.tail(), 2);

        assert_eq!(buffer.pop_front(), None);
    }
}
