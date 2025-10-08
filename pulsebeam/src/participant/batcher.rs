use std::{collections::VecDeque, net::SocketAddr};

pub struct Batcher {
    cap: usize,
    states: VecDeque<BatcherState>,
}

impl Batcher {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cap,
            states: VecDeque::with_capacity(3),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn push_back(&mut self, dst: SocketAddr, mut content: Vec<u8>) {
        for state in &mut self.states {
            match state.push(dst, content) {
                Some(bounced) => {
                    content = bounced;
                }
                None => return,
            }
        }

        // at this point, nobody took it.
        self.states
            .push_back(BatcherState::with_capacity(dst, self.cap));
    }

    pub fn pop_front(&mut self) -> Option<BatcherState> {
        self.states.pop_front()
    }

    pub fn front(&mut self) -> Option<&BatcherState> {
        self.states.front()
    }
}

pub struct BatcherState {
    pub dst: SocketAddr,
    pub segment_size: usize,
    is_tail: bool,
    pub buf: Vec<u8>,
}

impl BatcherState {
    fn with_capacity(dst: SocketAddr, cap: usize) -> Self {
        Self {
            dst,
            segment_size: 0,
            is_tail: false,
            buf: Vec::with_capacity(cap),
        }
    }

    fn push(&mut self, dst: SocketAddr, content: Vec<u8>) -> Option<Vec<u8>> {
        // Check if we can add content to current batch
        if self.dst != dst || (self.buf.len() + content.len()) > self.buf.capacity() {
            return Some(content);
        }

        // Set segment size only if it's the first segment (segment_size == 0)
        let is_first_segment = self.segment_size == 0;
        let same_segment = is_first_segment || self.segment_size == content.len();

        if is_first_segment && !content.is_empty() {
            self.segment_size = content.len();
        }

        // Add content if it's the same segment size or we're in tail mode
        if same_segment || self.is_tail {
            self.is_tail = !same_segment;
            self.buf.extend(content);
            None
        } else {
            Some(content)
        }
    }

    fn clear(&mut self) {
        self.segment_size = 0;
        self.is_tail = false;
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn create_test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080)
    }

    #[test]
    fn test_new_batcher_state() {
        let addr = create_test_addr();
        let batcher = BatcherState::with_capacity(addr, 100);

        assert_eq!(batcher.dst, addr);
        assert_eq!(batcher.segment_size, 0);
        assert!(!batcher.is_tail);
        assert_eq!(batcher.buf.capacity(), 100);
        assert_eq!(batcher.buf.len(), 0);
    }

    #[test]
    fn test_push_first_segment() {
        let addr = create_test_addr();
        let mut batcher = BatcherState::with_capacity(addr, 100);

        let content: Vec<u8> = vec![1, 2, 3];
        let result = batcher.push(addr, content.clone());

        assert_eq!(result, None);
        assert_eq!(batcher.segment_size, 3);
        assert!(!batcher.is_tail);
        assert_eq!(batcher.buf, content);
    }

    #[test]
    fn test_push_same_segment_size() {
        let addr = create_test_addr();
        let mut batcher = BatcherState::with_capacity(addr, 100);

        batcher.push(addr, vec![1, 2, 3]).unwrap();
        let result = batcher.push(addr, vec![4, 5, 6]);

        assert_eq!(result, None);
        assert_eq!(batcher.segment_size, 3);
        assert!(!batcher.is_tail);
        assert_eq!(batcher.buf, vec![1u8, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn test_push_different_segment_size() {
        let addr = create_test_addr();
        let mut batcher = BatcherState::with_capacity(addr, 100);

        batcher.push(addr, vec![1, 2, 3]).unwrap();
        let content: Vec<u8> = vec![4, 5];
        let result = batcher.push(addr, content.clone());

        assert_eq!(result, Some(content));
        assert_eq!(batcher.segment_size, 3);
        assert!(!batcher.is_tail);
        assert_eq!(batcher.buf, vec![1u8, 2, 3]);
    }

    #[test]
    fn test_push_different_destination() {
        let addr1 = create_test_addr();
        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 8080);
        let mut batcher = BatcherState::with_capacity(addr1, 100);

        let content: Vec<u8> = vec![1, 2, 3];
        let result = batcher.push(addr2, content.clone());

        assert_eq!(result, Some(content));
        assert_eq!(batcher.segment_size, 0);
        assert!(!batcher.is_tail);
        assert_eq!(batcher.buf, vec![] as Vec<u8>);
    }

    #[test]
    fn test_push_exceeds_capacity() {
        let addr = create_test_addr();
        let mut batcher = BatcherState::with_capacity(addr, 5);

        let content: Vec<u8> = vec![1, 2, 3, 4, 5, 6];
        let result = batcher.push(addr, content.clone());

        assert_eq!(result, Some(content));
        assert_eq!(batcher.segment_size, 0);
        assert!(!batcher.is_tail);
        assert_eq!(batcher.buf, vec![] as Vec<u8>);
    }

    #[test]
    fn test_clear() {
        let addr = create_test_addr();
        let mut batcher = BatcherState::with_capacity(addr, 100);

        batcher.push(addr, vec![1, 2, 3]).unwrap();
        batcher.is_tail = true;
        batcher.clear();

        assert_eq!(batcher.segment_size, 0);
        assert!(!batcher.is_tail);
        assert_eq!(batcher.buf, vec![] as Vec<u8>);
    }

    #[test]
    fn test_tail_mode() {
        let addr = create_test_addr();
        let mut batcher = BatcherState::with_capacity(addr, 100);

        batcher.push(addr, vec![1, 2, 3]).unwrap();
        batcher.push(addr, vec![4, 5]).unwrap();
        let result = batcher.push(addr, vec![6, 7, 8, 9]);

        assert_eq!(result, None);
        assert_eq!(batcher.segment_size, 3);
        assert!(!batcher.is_tail);
        assert_eq!(batcher.buf, vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9]);
    }
}
