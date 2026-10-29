//! GSO batching: many datagrams to one destination in one syscall.
//!
//! Overflow is explicit here: `#![deny(clippy::arithmetic_side_effects)]`. The
//! accounting decides how a contiguous arena is split into segments, so a
//! wrapped length does not fail — the kernel slices the buffer somewhere else
//! and the peer receives datagrams that were never sent.

use arrayvec::ArrayVec;
use pulsebeam_runtime::net;
use std::{collections::VecDeque, net::SocketAddr};

pub struct OwnedPacketQueue {
    max_segments: usize,
    packets: VecDeque<OwnedPacket>,
}

pub enum AppendStatus {
    Drained,
    Full,
}

pub trait NetworkEgress {
    fn append_udp(&mut self, packets: &mut OwnedPacketQueue) -> AppendStatus;
    fn flush(&mut self) -> bool;
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

    pub fn push_back(&mut self, dst: SocketAddr, contents: Vec<u8>) -> bool {
        if self.is_full() {
            return false;
        }
        debug_assert!(!contents.is_empty(), "Pushed content must not be empty");
        debug_assert!(
            contents.len() <= net::MAX_UDP_GSO_PAYLOAD_SIZE,
            "Packet exceeds the maximum framed payload"
        );
        self.packets.push_back(OwnedPacket { dst, contents });
        true
    }

    pub fn is_full(&self) -> bool {
        self.packets.len() >= net::BATCH_SIZE
    }

    pub fn discard(&mut self) {
        self.packets.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
}

#[derive(Clone, Copy)]
struct GsoPacketMeta {
    dst: SocketAddr,
    segment_size: usize,
    start: usize,
    end: usize,
}

#[derive(Default)]
struct UdpEgressCounters {
    submitted_datagrams: u64,
    submitted_bytes: u64,
    discarded_datagrams: u64,
    discarded_bytes: u64,
}

pub struct GsoSendBatch {
    arena: Vec<u8>,
    packets: ArrayVec<GsoPacketMeta, { net::BATCH_SIZE }>,
    counters: UdpEgressCounters,
}

impl GsoSendBatch {
    pub fn preallocated() -> Self {
        Self {
            arena: Vec::with_capacity(net::BATCH_SIZE * net::MAX_UDP_GSO_PAYLOAD_SIZE),
            packets: ArrayVec::new(),
            counters: UdpEgressCounters::default(),
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
        if first.contents.len() > net::MAX_UDP_PAYLOAD_SIZE {
            debug_assert!(false, "UDP packets must fit the configured MTU");
            return false;
        }

        let dst = first.dst;
        let segment_size = first.contents.len();
        let start = self.arena.len();
        let mut segment_count = 0;

        while let Some(packet) = queue.packets.front() {
            debug_assert!(!packet.contents.is_empty());
            if packet.contents.len() > net::MAX_UDP_PAYLOAD_SIZE {
                debug_assert!(false, "UDP packets must fit the configured MTU");
                break;
            }
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

    pub fn flush(&mut self, socket: &mut pulsebeam_udp::UdpSocket) -> bool {
        if self.packets.is_empty() {
            return false;
        }
        debug_assert!(self.packets.len() <= net::BATCH_SIZE);
        let mut packets = ArrayVec::<pulsebeam_udp::SendDatagram<'_>, { net::BATCH_SIZE }>::new();
        for packet in &self.packets {
            debug_assert!(packet.start < packet.end);
            debug_assert!(packet.end <= self.arena.len());
            debug_assert_ne!(packet.segment_size, 0);
            let Some(buf) = self.arena.get(packet.start..packet.end) else {
                debug_assert!(false, "queued packet escapes the arena");
                continue;
            };
            packets.push(pulsebeam_udp::SendDatagram {
                destination: packet.dst,
                payload: buf,
                gso_segment_size: (packet.segment_size < buf.len())
                    .then(|| {
                        u16::try_from(packet.segment_size)
                            .ok()
                            .and_then(std::num::NonZeroU16::new)
                    })
                    .flatten(),
            });
        }
        let report = socket.try_send_batch(&packets);
        drop(packets);
        self.account_submission(report.accepted);
        if report.accepted < self.packets.len() {
            tracing::trace!(
                accepted = report.accepted,
                offered = self.packets.len(),
                stop = ?report.stop,
                "UDP egress suffix discarded"
            );
        }
        // RTC output is irrevocable. The host discards the unsubmitted suffix
        // as local loss, never credits it as submitted and never replays it.
        self.packets.clear();
        self.arena.clear();
        report.accepted != 0
    }

    fn account_submission(&mut self, accepted: usize) {
        debug_assert!(accepted <= self.packets.len());
        for (index, packet) in self.packets.iter().enumerate() {
            let bytes = packet.end.saturating_sub(packet.start) as u64;
            let datagrams = packet
                .end
                .saturating_sub(packet.start)
                .div_ceil(packet.segment_size) as u64;
            if index < accepted {
                self.counters.submitted_bytes = self.counters.submitted_bytes.saturating_add(bytes);
                self.counters.submitted_datagrams =
                    self.counters.submitted_datagrams.saturating_add(datagrams);
            } else {
                self.counters.discarded_bytes = self.counters.discarded_bytes.saturating_add(bytes);
                self.counters.discarded_datagrams =
                    self.counters.discarded_datagrams.saturating_add(datagrams);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[test]
    fn udp_submission_accounts_only_accepted_entries_including_gso_short_tail() {
        let mut queue = OwnedPacketQueue::with_capacity(64);
        let first: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let second: SocketAddr = "127.0.0.1:2".parse().unwrap();
        assert!(queue.push_back(first, vec![1; 800]));
        assert!(queue.push_back(first, vec![2; 237]));
        assert!(queue.push_back(second, vec![3; 400]));
        let mut batch = GsoSendBatch::preallocated();
        assert!(batch.append_from(&mut queue));
        assert!(batch.append_from(&mut queue));
        batch.account_submission(1);
        assert_eq!(batch.counters.submitted_datagrams, 2);
        assert_eq!(batch.counters.submitted_bytes, 1037);
        assert_eq!(batch.counters.discarded_datagrams, 1);
        assert_eq!(batch.counters.discarded_bytes, 400);
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
}
