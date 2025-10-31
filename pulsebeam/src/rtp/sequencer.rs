use str0m::media::{Frequency, MediaTime};
use str0m::rtp::SeqNo;
use tokio::time::{Duration, Instant};

use crate::rtp::PacketTiming;
use std::collections::BTreeMap;
use std::fmt::Debug;

/// The number of packets to buffer while attempting to establish a new anchor.
const ACQUISITION_DEPTH: usize = 3;

/// The maximum time to wait for `ACQUISITION_DEPTH` packets to arrive before
/// creating a new anchor from whatever is available.
const ACQUISITION_TIMEOUT: Duration = Duration::from_millis(80);

/// The maximum number of packets to hold in the jitter buffer.
const JITTER_BUFFER_SIZE: usize = 512;

/// The maximum time to wait for a missing packet before declaring it lost.
const JITTER_MAX_WAIT: Duration = Duration::from_millis(200);

/// An incoming sequence number gap larger than this is considered non-recoverable
/// and will trigger an automatic reset of the stream anchor.
const MAX_SEQ_GAP: u64 = 3000;

/// The standard 90kHz clock rate for video RTP, used for all internal timestamp math.
const VIDEO_FREQUENCY: Frequency = Frequency::NINETY_KHZ;
const AUDIO_FREQUENCY: Frequency = Frequency::FORTY_EIGHT_KHZ;

/// A high-performance buffer for reordering RTP packets and handling loss.
#[derive(Debug)]
struct JitterBuffer<T> {
    buffer: BTreeMap<SeqNo, (T, Instant)>,
    next_seq_no_to_pop: SeqNo,
}

impl<T: PacketTiming> JitterBuffer<T> {
    fn new(start_seq: SeqNo) -> Self {
        JitterBuffer {
            buffer: BTreeMap::new(),
            next_seq_no_to_pop: start_seq,
        }
    }

    fn push(&mut self, packet: T) {
        self.buffer
            .insert(packet.seq_no(), (packet, Instant::now()));

        if self.buffer.len() > JITTER_BUFFER_SIZE {
            self.buffer.pop_first();
        }
    }

    fn pop_next(&mut self) -> Option<T> {
        // First, try to pop the packet we are expecting.
        if let Some((packet, _)) = self.buffer.remove(&self.next_seq_no_to_pop) {
            self.next_seq_no_to_pop = (*self.next_seq_no_to_pop).wrapping_add(1).into();
            return Some(packet);
        }

        // If it's not there, check for a stall condition.
        let mut stalled_seq = None;
        if let Some((&oldest_seq, (_, insertion_ts))) = self.buffer.first_key_value() {
            // A stall is defined as having waited too long for a packet that is now in the past.
            if insertion_ts.elapsed() > JITTER_MAX_WAIT && oldest_seq > self.next_seq_no_to_pop {
                stalled_seq = Some(oldest_seq);
            }
        }

        if let Some(oldest_seq) = stalled_seq {
            let (packet, _) = self.buffer.remove(&oldest_seq).unwrap();
            self.next_seq_no_to_pop = (*oldest_seq).wrapping_add(1).into();
            return Some(packet);
        }

        None
    }

    fn peek_first_packet(&self) -> Option<&T> {
        self.buffer.first_key_value().map(|(_, (packet, _))| packet)
    }

    fn clear(&mut self) {
        self.buffer.clear();
    }

    fn size(&self) -> usize {
        self.buffer.len()
    }

    fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

/// The mapping "contract" for a continuous segment of an RTP stream.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    stream_start_seq_in: SeqNo,
    stream_start_ts_in: MediaTime,
    rewritten_start_seq_out: SeqNo,
    rewritten_start_ts_out: MediaTime,
}

/// A generic, hardened RTP stream sequencer.
#[derive(Debug)]
pub struct RtpSequencer<T: PacketTiming + Debug> {
    buffer: JitterBuffer<T>,
    anchor: Option<Anchor>,
    last_output_seq: SeqNo,
    last_output_ts: MediaTime,
    last_forward_time: Instant,
    frequency: Frequency,
}

impl<T: PacketTiming + Debug> Default for RtpSequencer<T> {
    fn default() -> Self {
        Self::new(VIDEO_FREQUENCY)
    }
}

impl<T: PacketTiming + Debug> RtpSequencer<T> {
    pub fn video() -> Self {
        Self::new(VIDEO_FREQUENCY)
    }

    pub fn audio() -> Self {
        Self::new(AUDIO_FREQUENCY)
    }

    pub fn new(frequency: Frequency) -> Self {
        Self {
            frequency,
            buffer: JitterBuffer::new(0.into()),
            anchor: None,
            last_output_seq: 0.into(),
            last_output_ts: MediaTime::from_90khz(0),
            last_forward_time: Instant::now(),
        }
    }

    /// Pushes a new packet into the sequencer's jitter buffer. This operation is cheap.
    /// The main sequencing and rewriting logic is performed in `pop`.
    pub fn push(&mut self, packet: T, is_switch: bool) {
        if self.needs_reset(&packet, is_switch) {
            self.reset();
        }
        self.buffer.push(packet);
    }

    /// Pops the next sequenced packet.
    ///
    /// This method drives the internal state machine. It will return `None` if
    /// the sequencer is waiting for more packets to establish a sequence or if
    /// a gap in the sequence is detected. The caller should repeatedly call `pop`
    /// to drain the sequence.
    pub fn pop(&mut self) -> Option<(SeqNo, MediaTime, T)> {
        if self.anchor.is_none() {
            self.try_create_anchor();
            return None;
        }

        if let Some(packet) = self.buffer.pop_next() {
            let anchor = self.anchor.unwrap();

            let seq_offset = (*packet.seq_no()).wrapping_sub(*anchor.stream_start_seq_in);
            let rewritten_seq = (*anchor.rewritten_start_seq_out)
                .wrapping_add(seq_offset)
                .into();

            let ts_in_90khz = packet.rtp_timestamp().rebase(self.frequency);
            let ts_offset = ts_in_90khz
                .numer()
                .wrapping_sub(anchor.stream_start_ts_in.numer());
            let rewritten_ts = MediaTime::new(
                anchor
                    .rewritten_start_ts_out
                    .numer()
                    .wrapping_add(ts_offset),
                self.frequency,
            );

            self.last_output_seq = rewritten_seq;
            self.last_output_ts = rewritten_ts;
            self.last_forward_time = packet.arrival_timestamp();

            return Some((rewritten_seq, rewritten_ts, packet));
        }

        None
    }

    fn needs_reset(&self, packet: &T, is_switch: bool) -> bool {
        if is_switch {
            return true;
        }

        match self.anchor {
            None => false,
            Some(_) => {
                if self.buffer.is_empty() {
                    let diff = (*packet.seq_no()).wrapping_sub(*self.buffer.next_seq_no_to_pop);
                    diff > MAX_SEQ_GAP
                } else {
                    false
                }
            }
        }
    }

    fn reset(&mut self) {
        self.anchor = None;
        self.buffer.clear();
    }

    fn try_create_anchor(&mut self) {
        let buffer_full = self.buffer.size() >= ACQUISITION_DEPTH;
        let timeout = self.buffer.peek_first_packet().map_or(false, |p| {
            p.arrival_timestamp().elapsed() > ACQUISITION_TIMEOUT
        });

        if buffer_full || (timeout && !self.buffer.is_empty()) {
            if let Some(anchor_packet) = self.buffer.peek_first_packet() {
                let stream_start_seq_in = anchor_packet.seq_no();
                let stream_start_ts_in = anchor_packet.rtp_timestamp().rebase(self.frequency);
                let rewritten_start_seq_out = self.last_output_seq.wrapping_add(1).into();

                let time_since_last = anchor_packet
                    .arrival_timestamp()
                    .saturating_duration_since(self.last_forward_time);
                let time_gap_ticks =
                    duration_to_ticks(time_since_last, self.frequency.get() as u64);
                let rewritten_start_ts_out = MediaTime::new(
                    self.last_output_ts.numer().wrapping_add(time_gap_ticks),
                    self.frequency,
                );

                self.anchor = Some(Anchor {
                    stream_start_seq_in,
                    stream_start_ts_in,
                    rewritten_start_seq_out,
                    rewritten_start_ts_out,
                });

                self.buffer.next_seq_no_to_pop = stream_start_seq_in;
            }
        }
    }
}

fn duration_to_ticks(duration: Duration, clock_rate: u64) -> u64 {
    (duration.as_nanos() * clock_rate as u128 / 1_000_000_000) as u64
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct TestPacket {
        id: u64,
        seq: SeqNo,
        ts: MediaTime,
        arrival: Instant,
    }

    impl PacketTiming for TestPacket {
        fn seq_no(&self) -> SeqNo {
            self.seq
        }
        fn rtp_timestamp(&self) -> MediaTime {
            self.ts
        }
        fn arrival_timestamp(&self) -> Instant {
            self.arrival
        }
    }

    fn p(id: u64, seq: u64, arrival_ms: u64, start_time: Instant) -> TestPacket {
        TestPacket {
            id,
            seq: seq.into(),
            ts: MediaTime::from_90khz(seq * 3000), // 30fps
            arrival: start_time + Duration::from_millis(arrival_ms),
        }
    }

    #[test]
    fn test_simple_reordering() {
        let start = Instant::now();
        let mut seq = RtpSequencer::default();

        seq.push(p(1, 1, 0, start), false);
        seq.push(p(3, 3, 20, start), false);
        seq.push(p(2, 2, 30, start), false);

        assert_eq!(seq.pop().unwrap().2.id, 1);
        assert_eq!(seq.pop().unwrap().2.id, 2);
        assert_eq!(seq.pop().unwrap().2.id, 3);
        assert!(seq.pop().is_none());
    }

    #[tokio::test]
    async fn test_packet_loss_and_stall_recovery() {
        let start = Instant::now();
        let mut seq = RtpSequencer::default();

        seq.push(p(1, 1, 0, start), false);
        seq.push(p(3, 3, 20, start), false);
        seq.push(p(4, 4, 30, start), false);

        assert_eq!(seq.pop().unwrap().2.id, 1);
        assert!(seq.pop().is_none());

        tokio::time::sleep(JITTER_MAX_WAIT + Duration::from_millis(20)).await;

        // Pop does not have a packet yet but triggers internal checks.
        // The stall is detected when pop_next is called.
        assert!(seq.pop().is_none()); // This call might not yet detect the stall.

        // A subsequent pop will detect the stall because enough time has passed.
        let out1 = seq.pop().unwrap();
        let out2 = seq.pop().unwrap();

        assert_eq!(out1.2.id, 3);
        assert_eq!(*out1.0, 2_u64);
        assert_eq!(out2.2.id, 4);
        assert_eq!(*out2.0, 3_u64);
    }

    #[tokio::test]
    async fn test_layer_switch_with_lost_keyframe_start() {
        let start = Instant::now();
        let mut seq = RtpSequencer::default();

        seq.push(p(1, 100, 0, start), false);
        let (s1, _, p1) = seq.pop().unwrap();
        assert_eq!(*s1, 1_u64);
        assert_eq!(p1.id, 1);

        seq.push(p(3, 5002, 50, start), true);
        seq.push(p(2, 5001, 60, start), false);
        assert!(seq.pop().is_none());

        tokio::time::sleep(ACQUISITION_TIMEOUT + Duration::from_millis(10)).await;

        // The pop call will now trigger anchor creation.
        assert!(seq.pop().is_none());
        assert!(seq.anchor.is_some());

        // Now we can drain the packets.
        let (s2, _, p2) = seq.pop().unwrap();
        let (s3, _, p3) = seq.pop().unwrap();

        assert_eq!(*s2, 2_u64);
        assert_eq!(*s3, 3_u64);
        assert_eq!(p2.id, 2);
        assert_eq!(p3.id, 3);
    }

    #[test]
    fn test_automatic_reset_on_massive_gap() {
        let start = Instant::now();
        let mut seq = RtpSequencer::default();

        seq.push(p(1, 1, 0, start), false);
        assert_eq!(seq.pop().unwrap().2.id, 1);

        // A massive jump without an `is_switch` flag.
        seq.push(p(2, 1 + MAX_SEQ_GAP + 1, 50, start), false);

        assert!(seq.anchor.is_none());
        assert!(seq.pop().is_none());

        seq.push(p(3, 1 + MAX_SEQ_GAP + 2, 60, start), false);
        seq.push(p(4, 1 + MAX_SEQ_GAP + 3, 70, start), false);

        // The next pop will create the anchor.
        assert!(seq.pop().is_none());
        assert!(seq.anchor.is_some());

        let (s2, _, p2) = seq.pop().unwrap();
        let (s3, _, p3) = seq.pop().unwrap();
        let (s4, _, p4) = seq.pop().unwrap();

        assert_eq!(*s2, 2_u64);
        assert_eq!(p2.id, 2);
        assert_eq!(*s3, 3_u64);
        assert_eq!(p3.id, 3);
        assert_eq!(*s4, 4_u64);
        assert_eq!(p4.id, 4);
    }

    #[test]
    fn test_sequence_number_wrapping() {
        let start = Instant::now();
        let mut seq = RtpSequencer::default();
        let max = u16::MAX as u64;

        seq.push(p(1, max - 1, 0, start), false);
        seq.push(p(3, 0, 20, start), false);
        seq.push(p(2, max, 10, start), false);
        seq.push(p(4, 1, 30, start), false);

        let out1 = seq.pop().unwrap();
        let out2 = seq.pop().unwrap();
        let out3 = seq.pop().unwrap();
        let out4 = seq.pop().unwrap();

        assert_eq!(out1.2.seq, (max - 1).into());
        assert_eq!(*out1.0, 1_u64);
        assert_eq!(out2.2.seq, max.into());
        assert_eq!(*out2.0, 2_u64);
        assert_eq!(out3.2.seq, 0.into());
        assert_eq!(*out3.0, 3_u64);
        assert_eq!(out4.2.seq, 1.into());
        assert_eq!(*out4.0, 4_u64);
    }
}
