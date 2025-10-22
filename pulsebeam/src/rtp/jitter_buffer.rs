use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::Instant;

use str0m::{media::MediaTime, rtp::SeqNo};

use crate::rtp::PacketTiming;

const JITTER_SMOOTHING_FACTOR: f64 = 1.0 / 16.0;
const MAX_JITTER_SECONDS: f64 = 0.25;

#[derive(Debug)]
pub struct JitterBuffer<T: PacketTiming> {
    config: JitterBufferConfig,
    buffer: BTreeMap<SeqNo, T>,
    last_playout_seq: Option<SeqNo>,
    last_playout_rtp_ts: Option<MediaTime>,
    playout_time: Instant,
    jitter_estimate: f64,
    last_arrival_time: Option<Instant>,
    last_received_rtp_ts: Option<MediaTime>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PollResult<T: PacketTiming> {
    PacketReady(T),
    WaitUntil(Instant),
    Empty,
}

#[derive(Debug, Clone, Copy)]
pub struct JitterBufferConfig {
    pub max_capacity: usize,
    pub base_latency: Duration,
    pub jitter_multiplier: f64,
    pub loss_patience: Duration,
    pub silence_threshold: Duration,
}

impl JitterBufferConfig {
    pub fn video_interactive() -> Self {
        Self {
            base_latency: Duration::from_millis(10),
            jitter_multiplier: 3.0,
            loss_patience: Duration::from_millis(20),
            ..Self::default()
        }
    }

    pub fn audio_interactive() -> Self {
        Self {
            base_latency: Duration::from_millis(10),
            jitter_multiplier: 3.0,
            loss_patience: Duration::from_millis(40),
            ..Self::default()
        }
    }
}

impl Default for JitterBufferConfig {
    fn default() -> Self {
        Self {
            max_capacity: 512,
            base_latency: Duration::from_millis(20),
            jitter_multiplier: 4.0,
            loss_patience: Duration::from_millis(40),
            silence_threshold: Duration::from_secs(2),
        }
    }
}

impl<T: PacketTiming> JitterBuffer<T> {
    pub fn new(config: JitterBufferConfig) -> Self {
        tracing::info!(?config, "jitter buffer initialized");
        Self {
            config,
            buffer: BTreeMap::new(),
            last_playout_seq: None,
            last_playout_rtp_ts: None,
            playout_time: Instant::now(),
            jitter_estimate: 0.0,
            last_arrival_time: None,
            last_received_rtp_ts: None,
        }
    }

    pub fn push(&mut self, packet: T) {
        let seq = packet.seq_no();
        let arrival = packet.arrival_timestamp();
        tracing::trace!(sequence = *seq, "packet pushed");

        if let Some(last_seq) = self.last_playout_seq {
            if seq <= last_seq {
                tracing::debug!(
                    sequence = *seq,
                    last_seq = *last_seq,
                    "dropping old/duplicate packet"
                );
                metrics::counter!("jitterbuffer_dropped_packets_total").increment(1);
                return;
            }
        }

        if let Some(last_arrival) = self.last_arrival_time {
            if arrival.saturating_duration_since(last_arrival) > self.config.silence_threshold {
                tracing::warn!(threshold = ?self.config.silence_threshold, "silence detected, resetting jitter buffer");
                metrics::counter!("jitterbuffer_resets_total").increment(1);
                self.reset_state();
            }
        }

        self.update_jitter(&packet);

        if self.buffer.is_empty() && self.last_playout_seq.is_none() {
            self.playout_time = arrival + self.current_delay();
            tracing::debug!(playout_time = ?self.playout_time, "initial playout time established");
        }

        self.buffer.insert(seq, packet);
        metrics::histogram!("jitterbuffer_buffer_occupancy_packets")
            .record(self.buffer.len() as f64);

        if self.buffer.len() > self.config.max_capacity {
            if let Some(first_key) = self.buffer.keys().next().copied() {
                self.buffer.remove(&first_key);
                tracing::warn!(seq = *first_key, "buffer overflow, dropped oldest packet");
                metrics::counter!("jitterbuffer_dropped_packets_total").increment(1);
            }
        }
    }

    pub fn poll(&mut self, now: Instant) -> PollResult<T> {
        let next_seq_to_play = self.last_playout_seq.map_or_else(
            || self.buffer.keys().next().copied(),
            |last_played| Some(SeqNo::from(*last_played + 1)),
        );

        let Some(seq_to_play) = next_seq_to_play else {
            return PollResult::Empty;
        };

        if self.buffer.contains_key(&seq_to_play) {
            if now >= self.playout_time {
                let packet = self.buffer.remove(&seq_to_play).unwrap();
                self.advance_playout_time(&packet);
                self.last_playout_seq = Some(seq_to_play);
                self.last_playout_rtp_ts = Some(packet.rtp_timestamp());
                metrics::histogram!("jitterbuffer_induced_latency_seconds")
                    .record(self.current_delay().as_secs_f64());
                return PollResult::PacketReady(packet);
            } else {
                return PollResult::WaitUntil(self.playout_time);
            }
        }

        if !self.buffer.is_empty() {
            let overdue = now.saturating_duration_since(self.playout_time);
            if overdue > self.config.loss_patience {
                tracing::warn!(
                    seq_missing = *seq_to_play,
                    "loss patience exceeded, skipping gap"
                );
                metrics::counter!("jitterbuffer_gap_skips_total").increment(1);
                self.skip_gap(now);
                return self.poll(now);
            }
        }

        PollResult::WaitUntil(self.playout_time + self.config.loss_patience)
    }

    fn reset_state(&mut self) {
        self.buffer.clear();
        self.jitter_estimate = 0.0;
        self.last_playout_seq = None;
        self.last_playout_rtp_ts = None;
        self.last_arrival_time = None;
        self.last_received_rtp_ts = None;
        tracing::debug!("jitter buffer state reset");
    }

    fn update_jitter(&mut self, packet: &T) {
        let rtp_timestamp = packet.rtp_timestamp();
        if let (Some(last_arrival), Some(last_rtp)) =
            (self.last_arrival_time, self.last_received_rtp_ts)
        {
            let arrival_diff = packet
                .arrival_timestamp()
                .saturating_duration_since(last_arrival);
            let rtp_diff: Duration = rtp_timestamp.saturating_sub(last_rtp).into();

            let transit_diff = (arrival_diff.as_secs_f64() - rtp_diff.as_secs_f64()).abs();

            self.jitter_estimate += (transit_diff - self.jitter_estimate) * JITTER_SMOOTHING_FACTOR;
            self.jitter_estimate = self.jitter_estimate.clamp(0.0, MAX_JITTER_SECONDS);
        }

        metrics::histogram!("jitterbuffer_jitter_estimate_seconds").record(self.jitter_estimate);
        self.last_arrival_time = Some(packet.arrival_timestamp());
        self.last_received_rtp_ts = Some(rtp_timestamp);
    }

    fn advance_playout_time(&mut self, packet_sent: &T) {
        if let Some(next_packet) = self.buffer.values().next() {
            let rtp_diff = next_packet
                .rtp_timestamp()
                .saturating_sub(packet_sent.rtp_timestamp());
            self.playout_time += rtp_diff.into();
        }
    }

    fn skip_gap(&mut self, now: Instant) {
        if let Some(next_packet) = self.buffer.values().next() {
            self.playout_time = now;
            self.last_playout_seq = self
                .last_playout_seq
                .map(|_| SeqNo::from(*next_packet.seq_no() - 1));
            tracing::trace!(
                next_seq = *next_packet.seq_no(),
                "skipped gap to next packet"
            );
        }
    }

    fn current_delay(&self) -> Duration {
        let jitter_delay_secs = self.jitter_estimate * self.config.jitter_multiplier;
        let clamped = jitter_delay_secs.min(MAX_JITTER_SECONDS);
        self.config.base_latency + Duration::from_secs_f64(clamped)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::test::TestPacket;
    use str0m::media::{Frequency, MediaTime};
    use tracing_subscriber::fmt::Subscriber;

    fn setup() -> (JitterBuffer<TestPacket>, Instant) {
        let _ = Subscriber::builder()
            .with_max_level(tracing::Level::TRACE)
            .with_test_writer()
            .try_init();
        let now = Instant::now();
        let jb = JitterBuffer::new(JitterBufferConfig::default());
        (jb, now)
    }

    fn make_packet(
        seq_no: u64,
        rtp_ts_ms: u64,
        arrival_offset_ms: u64,
        now: Instant,
    ) -> TestPacket {
        TestPacket {
            seq_no: seq_no.into(),
            rtp_ts: MediaTime::from_millis(rtp_ts_ms),
            server_ts: now + Duration::from_millis(arrival_offset_ms),
        }
    }

    #[test]
    fn test_basic_poll() {
        let (mut jb, now) = setup();
        let packet = make_packet(1, 0, 0, now);
        jb.push(packet);

        assert!(matches!(jb.poll(now), PollResult::WaitUntil(_)));

        let playout_time = now + jb.current_delay() + Duration::from_millis(1);
        let res = jb.poll(playout_time);
        assert!(matches!(res, PollResult::PacketReady(_)));
    }

    #[test]
    fn test_reordering() {
        let (mut jb, now) = setup();
        let p2 = make_packet(2, 30, 30, now);
        let p1 = make_packet(1, 0, 0, now);
        jb.push(p2);
        jb.push(p1);

        let playout_time = now + jb.current_delay() + Duration::from_millis(1);

        let res1 = jb.poll(playout_time);
        assert!(matches!(res1, PollResult::PacketReady(p) if p.seq_no() == 1.into()));

        let res2 = jb.poll(playout_time + Duration::from_millis(30));
        assert!(matches!(res2, PollResult::PacketReady(p) if p.seq_no() == 2.into()));
    }

    #[test]
    fn test_gap_skip() {
        let (mut jb, now) = setup();
        jb.config.loss_patience = Duration::from_millis(30);

        let p1 = make_packet(1, 0, 0, now);
        jb.push(p1);
        let p3 = make_packet(3, 60, 60, now);
        jb.push(p3);

        let playout_time = now + jb.current_delay() + Duration::from_millis(1);
        let _ = jb.poll(playout_time);

        let skip_time = jb.playout_time + jb.config.loss_patience + Duration::from_millis(1);
        let res = jb.poll(skip_time);
        assert!(matches!(res, PollResult::PacketReady(p) if p.seq_no() == 3.into()));
    }

    #[test]
    fn test_silence_reset() {
        let (mut jb, now) = setup();
        jb.config.silence_threshold = Duration::from_millis(50);

        let p1 = make_packet(1, 0, 0, now);
        jb.push(p1);
        assert_ne!(jb.jitter_estimate, 0.0);

        let p2 = make_packet(2, 100, 100, now);
        jb.push(p2);
        assert_eq!(jb.jitter_estimate, 0.0);
        assert!(jb.last_playout_seq.is_none());
    }

    #[test]
    fn test_jitter_calculation() {
        let (mut jb, now) = setup();
        let freq = Frequency::new(1000).unwrap();

        let m1 = TestPacket {
            seq_no: 1.into(),
            rtp_ts: MediaTime::new(0, freq),
            server_ts: now,
        };
        jb.push(m1);
        assert_eq!(jb.jitter_estimate, 0.0);

        let m2 = TestPacket {
            seq_no: 2.into(),
            rtp_ts: MediaTime::new(30, freq),
            server_ts: now + Duration::from_millis(30),
        };
        jb.push(m2);
        assert!(jb.jitter_estimate < 1e-9);

        let m3 = TestPacket {
            seq_no: 3.into(),
            rtp_ts: MediaTime::new(60, freq),
            server_ts: now + Duration::from_millis(70),
        };
        jb.push(m3);
        assert!(jb.jitter_estimate > 0.005);
    }
}
