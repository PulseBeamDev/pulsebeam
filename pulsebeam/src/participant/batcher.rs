//! GSO batching: many datagrams to one destination in one syscall.
//!
//! Overflow is explicit here: `#![deny(clippy::arithmetic_side_effects)]`. The
//! accounting decides how a contiguous arena is split into segments, so a
//! wrapped length does not fail — the kernel slices the buffer somewhere else
//! and the peer receives datagrams that were never sent.

use arrayvec::ArrayVec;
use pulsebeam_runtime::net;
use std::{
    collections::VecDeque,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

const MAX_FREE_STATES: usize = 3;

pub struct OwnedPacketQueue {
    max_segments: usize,
    packets: VecDeque<OwnedPacket>,
}

struct OwnedPacket {
    dst: SocketAddr,
    contents: Vec<u8>,
}

impl OwnedPacketQueue {
    pub fn with_capacity(max_segments: usize) -> Self {
        debug_assert_ne!(max_segments, 0);
        Self {
            max_segments,
            packets: VecDeque::with_capacity(max_segments),
        }
    }

    pub fn push_back(&mut self, dst: SocketAddr, contents: Vec<u8>) {
        debug_assert!(!contents.is_empty(), "Pushed content must not be empty");
        debug_assert!(
            contents.len() <= net::MAX_UDP_PAYLOAD_SIZE,
            "Packet exceeds maximum supported MTU"
        );
        self.packets.push_back(OwnedPacket { dst, contents });
    }
}

#[derive(Clone, Copy)]
struct GsoPacketMeta {
    dst: SocketAddr,
    segment_size: usize,
    start: usize,
    end: usize,
}

pub struct GsoSendBatch {
    arena: Vec<u8>,
    packets: ArrayVec<GsoPacketMeta, { net::BATCH_SIZE }>,
}

impl GsoSendBatch {
    pub fn preallocated() -> Self {
        Self {
            arena: Vec::with_capacity(net::BATCH_SIZE * net::MAX_UDP_GSO_PAYLOAD_SIZE),
            packets: ArrayVec::new(),
        }
    }

    pub fn is_full(&self) -> bool {
        self.packets.len() == self.packets.capacity()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    pub fn append_from(&mut self, queue: &mut OwnedPacketQueue) -> bool {
        debug_assert!(!self.is_full());
        let Some(first) = queue.packets.front() else {
            return false;
        };
        debug_assert!(!first.contents.is_empty());
        debug_assert!(first.contents.len() <= net::MAX_UDP_PAYLOAD_SIZE);

        let dst = first.dst;
        let segment_size = first.contents.len();
        let start = self.arena.len();
        let mut segment_count = 0;

        while let Some(packet) = queue.packets.front() {
            debug_assert!(!packet.contents.is_empty());
            debug_assert!(packet.contents.len() <= net::MAX_UDP_PAYLOAD_SIZE);
            debug_assert!(segment_count <= queue.max_segments);
            debug_assert!(self.arena.len().saturating_sub(start) <= net::MAX_UDP_GSO_PAYLOAD_SIZE);

            if packet.dst != dst
                || segment_count >= queue.max_segments
                || self
                    .arena
                    .len()
                    .saturating_sub(start)
                    .saturating_add(packet.contents.len())
                    > net::MAX_UDP_GSO_PAYLOAD_SIZE
                || packet.contents.len() > segment_size
            {
                break;
            }

            let is_tail = packet.contents.len() < segment_size;
            self.arena.extend_from_slice(&packet.contents);
            segment_count = segment_count.saturating_add(1);
            queue.packets.pop_front();
            if is_tail {
                break;
            }
        }

        let end = self.arena.len();
        debug_assert_ne!(segment_count, 0);
        debug_assert!(end > start);
        debug_assert!(end.saturating_sub(start) <= net::MAX_UDP_GSO_PAYLOAD_SIZE);
        self.packets.push(GsoPacketMeta {
            dst,
            segment_size,
            start,
            end,
        });
        true
    }

    pub fn flush(&mut self, socket: &mut net::UnifiedSocket) {
        if self.packets.is_empty() {
            return;
        }
        debug_assert!(self.packets.len() <= net::BATCH_SIZE);
        let mut packets = ArrayVec::<net::SendPacket<'_>, { net::BATCH_SIZE }>::new();
        for packet in &self.packets {
            debug_assert!(packet.start < packet.end);
            debug_assert!(packet.end <= self.arena.len());
            debug_assert_ne!(packet.segment_size, 0);
            let Some(buf) = self.arena.get(packet.start..packet.end) else {
                debug_assert!(false, "queued packet escapes the arena");
                continue;
            };
            packets.push(net::SendPacket {
                dst: packet.dst,
                buf,
                segment_size: packet.segment_size,
            });
        }
        let batch = net::SendPacketBatch { packets: &packets };
        if let Err(err) = socket.try_send_batch(&batch) {
            tracing::trace!(error = ?err, "error writing UDP egress batch");
        }
        drop(packets);
        self.packets.clear();
        self.arena.clear();
        debug_assert!(self.arena.capacity() >= net::BATCH_SIZE * net::MAX_UDP_GSO_PAYLOAD_SIZE);
    }
}

/// Manages a pool of `BatcherState` objects to build GSO-compatible datagrams efficiently.
pub struct Batcher {
    cap: usize,
    active_states: VecDeque<BatcherState>,
    free_states: Vec<BatcherState>,
}

impl Batcher {
    /// Creates a new `Batcher` where each internal buffer has the specified capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            // The socket reports the kernel's actual UDP_SEGMENT fan-out
            // limit. Do not silently reduce it here: doing so turns a 64
            // segment GSO-capable socket into eight-datagram submissions.
            cap,
            active_states: VecDeque::with_capacity(3),
            free_states: Vec::with_capacity(MAX_FREE_STATES),
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.active_states.is_empty()
    }

    /// Pushes a content slice into an appropriate batch.
    ///
    /// It attempts to find an existing batch for the same destination that is not yet sealed.
    /// If no suitable batch is found, it takes one from the free pool or allocates a new one.
    pub fn push_back(&mut self, dst: SocketAddr, content: &[u8]) {
        debug_assert!(!content.is_empty(), "Pushed content must not be empty");

        if let Some(state) = self.active_states.back_mut()
            && state.try_push(dst, content)
        {
            return;
        }

        let mut new_state = match self.free_states.pop() {
            Some(state) => state,
            None => BatcherState::with_capacity(self.cap),
        };

        new_state.reset(dst);

        if new_state.try_push(dst, content) {
            self.active_states.push_back(new_state);
        } else {
            self.free_states.push(new_state);
            debug_assert!(
                false,
                "Content is larger than the batcher's configured capacity"
            );
        }
    }

    /// Pops a single batch from the front of the queue.
    pub fn pop_front(&mut self) -> Option<BatcherState> {
        self.active_states.pop_front()
    }

    pub fn front(&mut self) -> Option<&BatcherState> {
        self.active_states.front()
    }

    /// Reclaims a `BatcherState`, returning its memory to the pool for future reuse.
    pub fn reclaim(&mut self, state: BatcherState) {
        if self.free_states.len() < self.free_states.capacity() {
            self.free_states.push(state);
        }
    }

    /// Exposes every completed GSO datagram without copying its payload.
    /// The shard gathers these from all dirty participants into one
    /// `sendmmsg()` submission, then calls `discard_all()` after the
    /// lossy egress decision has been made.
    pub fn packets(&self) -> impl Iterator<Item = net::SendPacket<'_>> + '_ {
        self.active_states.iter().map(|state| net::SendPacket {
            dst: state.dst,
            buf: &state.buf,
            segment_size: state.segment_size,
        })
    }

    /// Releases every queued packet after the output phase.  UDP/TCP egress
    /// is deliberately lossy, so a short send or `WouldBlock` also drains the
    /// queue instead of retaining latency-inducing backlog.
    pub fn discard_all(&mut self) {
        while let Some(state) = self.pop_front() {
            self.reclaim(state);
        }
    }

    pub fn flush_tcp(&mut self, socket: &mut net::tcp::TcpTransport) {
        while let Some(state) = self.front() {
            debug_assert!(state.segment_count > 0, "Attempted to flush an empty batch");
            debug_assert!(
                state.segment_size != 0,
                "BatcherState must have a nonzero segment_size before flush"
            );
            debug_assert!(
                state.buf.len() <= state.max_segments.saturating_mul(net::MAX_UDP_PAYLOAD_SIZE),
                "Batch exceeds configured TCP batch capacity"
            );
            let packet = [net::SendPacket {
                dst: state.dst,
                buf: &state.buf,
                segment_size: state.segment_size,
            }];
            if let Err(err) = socket.try_send_batch(&net::SendPacketBatch { packets: &packet }) {
                tracing::trace!("error on writing to TCP socket: {:?}", err);
            }
            // Reclaimed either way: a failed write drops the batch rather than
            // retrying it, so leaving it queued would spin this loop forever.
            let Some(state) = self.pop_front() else {
                break;
            };
            self.reclaim(state);
        }
    }
}

/// Holds the state for a single GSO-compatible batch.
pub struct BatcherState {
    pub dst: SocketAddr,
    pub segment_size: usize,
    segment_count: usize,
    max_segments: usize,
    sealed: bool,
    pub buf: Vec<u8>,
}

impl BatcherState {
    fn with_capacity(cap: usize) -> Self {
        debug_assert_ne!(cap, 0);
        Self {
            dst: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            segment_size: 0,
            segment_count: 0,
            max_segments: cap,
            sealed: false,
            // Reserving the full GSO maximum for every participant is costly
            // at SFU scale. Grow only for the batches that are actually used;
            // recycled states retain that capacity for the next tick.
            //
            // Capped at the GSO ceiling because `try_push` refuses to go past
            // it, so any capacity above ~43 segments would reserve more than
            // the batch can ever hold.
            buf: Vec::with_capacity(
                cap.saturating_mul(net::MAX_UDP_PAYLOAD_SIZE)
                    .min(net::MAX_UDP_GSO_PAYLOAD_SIZE),
            ),
        }
    }

    /// Attempts to append a content slice to the buffer. Returns true on success.
    fn try_push(&mut self, dst: SocketAddr, content: &[u8]) -> bool {
        debug_assert!(!content.is_empty(), "Segment content must not be empty");
        debug_assert!(
            content.len() <= net::MAX_UDP_PAYLOAD_SIZE,
            "Segment content exceeds maximum supported MTU"
        );
        debug_assert_eq!(self.buf.is_empty(), self.segment_count == 0);
        debug_assert_eq!(self.segment_size == 0, self.segment_count == 0);
        debug_assert!(self.segment_count <= self.max_segments);
        debug_assert!(self.buf.len() <= net::MAX_UDP_GSO_PAYLOAD_SIZE);

        if self.sealed {
            return false;
        }

        if self.dst != dst {
            return false;
        }

        if self.segment_count >= self.max_segments {
            return false;
        }

        if self.buf.len().saturating_add(content.len()) > net::MAX_UDP_GSO_PAYLOAD_SIZE {
            return false;
        }

        if self.segment_size == 0 {
            self.segment_size = content.len();
        }

        if content.len() == self.segment_size {
            debug_assert!(
                self.buf.len().saturating_add(content.len())
                    <= self.max_segments.saturating_mul(net::MAX_UDP_PAYLOAD_SIZE)
            );
            self.buf.extend_from_slice(content);
            self.segment_count = self.segment_count.saturating_add(1);
            debug_assert!(self.segment_count <= self.max_segments);
            debug_assert!(self.buf.len() <= net::MAX_UDP_GSO_PAYLOAD_SIZE);
            true
        } else if content.len() < self.segment_size {
            debug_assert!(
                self.buf.len().saturating_add(content.len())
                    <= self.max_segments.saturating_mul(net::MAX_UDP_PAYLOAD_SIZE)
            );
            self.buf.extend_from_slice(content);
            self.segment_count = self.segment_count.saturating_add(1);
            self.sealed = true;
            debug_assert!(self.segment_count <= self.max_segments);
            debug_assert!(self.buf.len() <= net::MAX_UDP_GSO_PAYLOAD_SIZE);
            true
        } else {
            false
        }
    }

    /// Resets the state's properties for reuse.
    fn reset(&mut self, dst: SocketAddr) {
        self.dst = dst;
        self.segment_size = 0;
        self.segment_count = 0;
        self.sealed = false;
        self.buf.clear();
        debug_assert_eq!(self.segment_size, 0);
        debug_assert_eq!(self.segment_count, 0);
        debug_assert!(self.buf.is_empty());
    }
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::string_slice,
        clippy::indexing_slicing
    )]
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    #![allow(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::float_cmp
    )]
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    /// GSO segment accounting at its edges.
    ///
    /// The invariant is "every segment the same size, except a smaller final
    /// one that seals the batch". The accounting is `buf.len() + content.len()`
    /// against a 65535-byte ceiling and `segment_count` against capacity, so
    /// the cases that matter are exactly at, one under and one over each — not
    /// a plausible run of 1200-byte packets, which never approaches either.
    mod segment_accounting {
        use super::super::*;
        use super::create_test_addr;

        fn state(cap: usize) -> BatcherState {
            let mut st = BatcherState::with_capacity(cap);
            st.reset(create_test_addr());
            st
        }

        #[test]
        fn a_batch_fills_to_exactly_its_segment_capacity() {
            let mut st = state(4);
            let dst = create_test_addr();
            for i in 0..4 {
                assert!(st.try_push(dst, &[7u8; 100]), "segment {i} should fit");
            }
            assert!(!st.try_push(dst, &[7u8; 100]), "capacity is a hard stop");
            assert_eq!(st.segment_count, 4);
            assert_eq!(st.buf.len(), 400);
        }

        /// The GSO payload ceiling, approached from both sides. A 1400-byte
        /// segment divides 65535 unevenly, which is the realistic shape.
        #[test]
        fn the_gso_payload_ceiling_is_never_exceeded() {
            let seg = 1400usize;
            let mut st = state(1_000);
            let dst = create_test_addr();
            let mut pushed = 0usize;
            while st.try_push(dst, &vec![1u8; seg]) {
                pushed += 1;
                assert!(
                    st.buf.len() <= net::MAX_UDP_GSO_PAYLOAD_SIZE,
                    "buffer passed the GSO ceiling at segment {pushed}"
                );
            }
            assert_eq!(pushed, net::MAX_UDP_GSO_PAYLOAD_SIZE / seg);
            assert!(st.buf.len() + seg > net::MAX_UDP_GSO_PAYLOAD_SIZE);
        }

        #[test]
        fn a_shorter_final_segment_seals_the_batch() {
            let mut st = state(8);
            let dst = create_test_addr();
            assert!(st.try_push(dst, &[1u8; 200]));
            assert!(st.try_push(dst, &[1u8; 199]), "a shorter tail is allowed");
            assert!(st.sealed);
            assert!(
                !st.try_push(dst, &[1u8; 199]),
                "nothing follows a sealed batch, even at the same size"
            );
            assert_eq!(st.segment_count, 2);
        }

        /// One byte either side of the established segment size: equal fits,
        /// smaller seals, larger is refused outright.
        #[test]
        fn segment_size_boundaries_are_exact() {
            let dst = create_test_addr();

            let mut equal = state(4);
            assert!(equal.try_push(dst, &[1u8; 300]));
            assert!(equal.try_push(dst, &[1u8; 300]));
            assert!(!equal.sealed);

            let mut smaller = state(4);
            assert!(smaller.try_push(dst, &[1u8; 300]));
            assert!(smaller.try_push(dst, &[1u8; 299]));
            assert!(smaller.sealed);

            let mut larger = state(4);
            assert!(larger.try_push(dst, &[1u8; 300]));
            assert!(
                !larger.try_push(dst, &[1u8; 301]),
                "a longer segment cannot join"
            );
            assert_eq!(larger.segment_count, 1);
        }

        #[test]
        fn a_different_destination_never_joins_a_batch() {
            let mut st = state(4);
            let dst = create_test_addr();
            assert!(st.try_push(dst, &[1u8; 100]));
            let other = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)), 4444);
            assert!(!st.try_push(other, &[1u8; 100]));
            assert_eq!(st.segment_count, 1);
        }

        /// `with_capacity` multiplies capacity by the MTU to size the arena. A
        /// capacity this large has no business being allocated, but it must not
        /// wrap into a small reservation either.
        #[test]
        fn an_absurd_capacity_does_not_wrap_the_arena_reservation() {
            let st = BatcherState::with_capacity(usize::MAX);
            assert!(st.buf.capacity() <= net::MAX_UDP_GSO_PAYLOAD_SIZE);
        }
    }

    fn create_test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080)
    }

    #[test]
    fn owned_packets_fill_preallocated_send_batch() {
        let addr = create_test_addr();
        let mut packets = OwnedPacketQueue::with_capacity(4);
        packets.push_back(addr, vec![1; 1000]);
        packets.push_back(addr, vec![2; 1000]);
        packets.push_back(addr, vec![3; 500]);
        let mut output = GsoSendBatch::preallocated();

        assert!(output.append_from(&mut packets));
        assert_eq!(output.packets[0].dst, addr);
        assert_eq!(output.packets[0].segment_size, 1000);
        assert_eq!(output.packets[0].end - output.packets[0].start, 2500);
        assert!(!output.append_from(&mut packets));
    }

    #[test]
    fn send_batch_packs_datagrams_into_one_contiguous_arena() {
        let first_addr = create_test_addr();
        let second_addr = SocketAddr::new(first_addr.ip(), first_addr.port() + 1);
        let mut packets = OwnedPacketQueue::with_capacity(4);
        packets.push_back(first_addr, vec![1; 1000]);
        packets.push_back(first_addr, vec![2; 500]);
        packets.push_back(second_addr, vec![3; 700]);
        let mut batch = GsoSendBatch::preallocated();
        let capacity = batch.arena.capacity();

        assert!(batch.append_from(&mut packets));
        assert!(batch.append_from(&mut packets));

        assert_eq!(batch.packets.len(), 2);
        assert_eq!(batch.packets[0].start, 0);
        assert_eq!(batch.packets[0].end, 1500);
        assert_eq!(batch.packets[1].start, batch.packets[0].end);
        assert_eq!(batch.packets[1].end, 2200);
        assert_eq!(batch.arena.capacity(), capacity);
    }

    #[test]
    fn owned_packets_preserve_incompatible_packet_for_next_buffer() {
        let addr = create_test_addr();
        let other = SocketAddr::new(addr.ip(), addr.port() + 1);
        let mut packets = OwnedPacketQueue::with_capacity(4);
        packets.push_back(addr, vec![1; 500]);
        packets.push_back(addr, vec![2; 600]);
        packets.push_back(other, vec![3; 600]);

        let mut batch = GsoSendBatch::preallocated();
        assert!(batch.append_from(&mut packets));
        assert!(batch.append_from(&mut packets));
        assert!(batch.append_from(&mut packets));

        assert_eq!(&batch.arena[0..500], &[1; 500]);
        assert_eq!(&batch.arena[500..1100], &[2; 600]);
        assert_eq!(&batch.arena[1100..1700], &[3; 600]);
        assert_eq!(batch.packets[2].dst, other);
    }

    #[test]
    fn test_appends_same_size_and_stays_open() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(4);

        batcher.push_back(addr, &[1; 1000]);
        batcher.push_back(addr, &[2; 1000]);

        assert_eq!(batcher.active_states.len(), 1);
        let batch = &batcher.active_states[0];
        assert!(!batch.sealed);
        assert_eq!(batch.segment_size, 1000);
        assert_eq!(batch.buf.len(), 2000);
    }

    #[test]
    fn test_uses_the_socket_reported_gso_capacity() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(16);

        for _ in 0..16 {
            batcher.push_back(addr, &[1; 1000]);
        }

        assert_eq!(batcher.active_states.len(), 1);
        assert_eq!(batcher.active_states[0].segment_count, 16);
        assert_eq!(batcher.active_states[0].max_segments, 16);
    }

    #[test]
    fn test_gso_payload_limit_seals_before_segment_limit() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(64);
        let segment = [0; net::MAX_UDP_PAYLOAD_SIZE];
        let count = net::MAX_UDP_GSO_PAYLOAD_SIZE / segment.len();

        for _ in 0..=count {
            batcher.push_back(addr, &segment);
        }

        assert_eq!(batcher.active_states.len(), 2);
        assert_eq!(batcher.active_states[0].segment_count, count);
        assert_eq!(batcher.active_states[1].segment_count, 1);
    }

    #[test]
    fn test_gso_payload_limit_scales_with_segment_size() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(64);
        let segment = [0; 1200];
        let count = net::MAX_UDP_GSO_PAYLOAD_SIZE / segment.len();

        for _ in 0..=count {
            batcher.push_back(addr, &segment);
        }

        assert_eq!(batcher.active_states[0].segment_count, count);
        assert_eq!(batcher.active_states[1].segment_count, 1);
    }

    #[test]
    fn test_appends_tail_and_seals() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(4);

        batcher.push_back(addr, &[1; 1000]);
        batcher.push_back(addr, &[2; 1000]);
        batcher.push_back(addr, &[3; 500]); // The tail packet

        assert_eq!(batcher.active_states.len(), 1);
        let batch = &batcher.active_states[0];
        assert!(batch.sealed);
        assert_eq!(batch.segment_size, 1000);
        assert_eq!(batch.buf.len(), 2500);
    }

    #[test]
    fn test_sealed_batch_rejects_pushes_creating_new_batch() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(4);

        batcher.push_back(addr, &[1; 1000]);
        batcher.push_back(addr, &[3; 500]); // This seals the first batch

        // A further push should be rejected and create a new batch
        batcher.push_back(addr, &[4; 1000]);
        assert_eq!(batcher.active_states.len(), 2);

        let batch1 = &batcher.active_states[0];
        let batch2 = &batcher.active_states[1];

        assert_eq!(batch1.buf.len(), 1500);
        assert!(batch1.sealed);
        assert_eq!(batch2.buf.len(), 1000);
        assert!(!batch2.sealed);
    }

    #[test]
    fn test_reclaim_and_reuse_resets_sealed_state() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(4);

        // Create a batch and seal it
        batcher.push_back(addr, &[1; 100]);
        batcher.push_back(addr, &[2; 50]);

        let sealed_batch = batcher.pop_front().unwrap();
        assert!(sealed_batch.sealed);
        assert!(batcher.is_empty());

        // Reclaim the sealed state
        batcher.reclaim(sealed_batch);

        // Push again, which should reuse the reclaimed state from the pool
        batcher.push_back(addr, &[3; 200]);
        assert_eq!(batcher.active_states.len(), 1);
        let reused_batch = &batcher.active_states[0];

        assert!(!reused_batch.sealed, "Reused batch should be open");
        assert_eq!(reused_batch.segment_size, 200);
        assert_eq!(reused_batch.buf.len(), 200);
    }

    #[test]
    fn test_pool_miss_allocates_new_state() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(4);
        assert_eq!(batcher.free_states.len(), 0);

        // This is a pool miss
        batcher.push_back(addr, &[1; 10]);
        assert_eq!(batcher.active_states.len(), 1);
        assert_eq!(batcher.free_states.len(), 0);
    }

    #[test]
    fn test_pool_hit_reuses_state() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(1);

        // First push causes allocation
        batcher.push_back(addr, &[1; 10]);
        let state = batcher.pop_front().unwrap();
        batcher.reclaim(state);
        assert_eq!(batcher.free_states.len(), 1);

        // Second push should be a pool hit
        batcher.push_back(addr, &[2; 20]);
        assert_eq!(batcher.active_states.len(), 1);
        assert_eq!(batcher.free_states.len(), 0);
        let state = batcher.pop_front().unwrap();
        assert_eq!(state.buf, [2; 20]);
        assert_eq!(state.segment_size, 20);
        assert_eq!(state.dst, addr);
        batcher.reclaim(state);

        // Third shrinks the content
        batcher.push_back(addr, &[3; 5]);
        assert_eq!(batcher.active_states.len(), 1);
        assert_eq!(batcher.free_states.len(), 0);
        let state = batcher.pop_front().unwrap();
        assert_eq!(state.buf, [3; 5]);
        assert_eq!(state.segment_size, 5);
        assert_eq!(state.dst, addr);
        batcher.reclaim(state);
    }

    #[test]
    fn test_seal_unequal_size() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(3);

        // First push causes allocation
        batcher.push_back(addr, &[1; 10]);
        batcher.push_back(addr, &[2; 10]);
        // This is larger than last segment, it shouldn't be allowed
        batcher.push_back(addr, &[3; 11]);
        batcher.push_back(addr, &[4; 11]);
        batcher.push_back(addr, &[5; 11]);
        assert_eq!(batcher.active_states.len(), 2);
        let batch = batcher.pop_front().unwrap();
        assert_eq!(batch.buf.len(), 20);
        let batch = batcher.pop_front().unwrap();
        assert_eq!(batch.buf.len(), 33);

        batcher.push_back(addr, &[1; 10]);
        batcher.push_back(addr, &[2; 10]);
        batcher.push_back(addr, &[3; 9]);
        batcher.push_back(addr, &[4; 9]);
        batcher.push_back(addr, &[5; 9]);
        assert_eq!(batcher.active_states.len(), 2);
        let batch = batcher.pop_front().unwrap();
        assert_eq!(batch.buf.len(), 29);
        let batch = batcher.pop_front().unwrap();
        assert_eq!(batch.buf.len(), 18);
    }

    #[test]
    fn test_segment_capacity_limits_and_batch_creation() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(2);

        batcher.push_back(addr, &[1; 500]);
        batcher.push_back(addr, &[2; 500]);
        batcher.push_back(addr, &[3; 500]);

        assert_eq!(batcher.active_states.len(), 2);
        let first = batcher.pop_front().unwrap();
        assert_eq!(first.segment_count, 2);
        assert_eq!(first.buf.len(), 1000);
        assert!(!first.sealed);

        let second = batcher.pop_front().unwrap();
        assert_eq!(second.segment_count, 1);
        assert_eq!(second.buf.len(), 500);
        assert_eq!(second.segment_size, 500);
    }

    #[test]
    fn test_batcher_seals_on_smaller_tail_packet() {
        let addr = create_test_addr();
        let mut batcher = Batcher::with_capacity(3);

        batcher.push_back(addr, &[1; 1000]);
        batcher.push_back(addr, &[2; 1000]);
        batcher.push_back(addr, &[3; 500]);

        assert_eq!(batcher.active_states.len(), 1);
        let batch = &batcher.active_states[0];
        assert!(batch.sealed);
        assert_eq!(batch.segment_size, 1000);
        assert_eq!(batch.segment_count, 3);
        assert_eq!(batch.buf.len(), 2500);
    }
}
