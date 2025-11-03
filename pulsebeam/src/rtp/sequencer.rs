use std::collections::BTreeMap;
use str0m::media::{Frequency, MediaTime};
use str0m::rtp::{SeqNo, Ssrc};

use crate::rtp::{Packet, TimingHeader};

/// The standard 90kHz clock rate for video RTP, used for all internal timestamp math.
const VIDEO_FREQUENCY: Frequency = Frequency::NINETY_KHZ;
const AUDIO_FREQUENCY: Frequency = Frequency::FORTY_EIGHT_KHZ;

pub struct RtpSequencer<T> {
    state: SequencerState<T>,
}

impl<T: Packet> RtpSequencer<T> {
    pub fn video() -> Self {
        Self::new(VIDEO_FREQUENCY)
    }

    pub fn audio() -> Self {
        Self::new(AUDIO_FREQUENCY)
    }

    pub fn new(frequency: Frequency) -> Self {
        let timeline = Timeline::new(frequency);
        let buffer = KeyframeBuffer::new();
        let state = NewState {
            timeline,
            keyframe_buffer: buffer,
        };
        Self {
            state: SequencerState::New(state),
        }
    }

    pub fn is_stable(&self) -> bool {
        matches!(&self.state, SequencerState::Stable(_))
    }

    pub fn push(&mut self, packet: &T) {
        let packet = packet.clone();
        let new_state = match std::mem::replace(&mut self.state, SequencerState::Invalid) {
            SequencerState::New(state) => state.process(packet),
            SequencerState::Stable(state) => state.process(packet),
            SequencerState::Switching(state) => state.process(packet),
            SequencerState::Invalid => unreachable!(),
        };
        self.state = new_state;
    }

    pub fn pop(&mut self) -> Option<(TimingHeader, T)> {
        match &mut self.state {
            SequencerState::Stable(state) => state.poll(),
            SequencerState::Switching(state) => state.poll(),
            _ => None,
        }
    }
}

struct StreamState {
    ssrc: Ssrc,
    seq_no: SeqNo,
    rtp_ts: MediaTime,
}

enum SequencerState<T> {
    New(NewState<T>),
    Stable(StableState<T>),
    Switching(SwitchingState<T>),
    Invalid,
}

struct NewState<T> {
    timeline: Timeline,
    keyframe_buffer: KeyframeBuffer<T>,
}

impl<T: Packet> NewState<T> {
    fn process(mut self, packet: T) -> SequencerState<T> {
        let new_ssrc = packet.ssrc();
        if self.keyframe_buffer.push(packet) {
            self.timeline.mark_need_rebase();
            SequencerState::Stable(StableState {
                pending: None,
                keyframe_buffer: self.keyframe_buffer,
                timeline: self.timeline,
                active_ssrc: new_ssrc,
            })
        } else {
            SequencerState::New(self)
        }
    }
}

struct StableState<T> {
    pending: Option<T>,
    timeline: Timeline,
    keyframe_buffer: KeyframeBuffer<T>,

    active_ssrc: Ssrc,
}

impl<T: Packet> StableState<T> {
    fn process(mut self, packet: T) -> SequencerState<T> {
        if packet.ssrc() != self.active_ssrc {
            let new_ssrc = packet.ssrc();
            self.keyframe_buffer.clear();
            self.keyframe_buffer.push(packet);
            SequencerState::Switching(SwitchingState {
                pending: self.pending,
                keyframe_buffer: self.keyframe_buffer,
                timeline: self.timeline,
                new_ssrc,
                old_ssrc: self.active_ssrc,
            })
        } else {
            self.pending.replace(packet);
            SequencerState::Stable(self)
        }
    }

    fn poll(&mut self) -> Option<(TimingHeader, T)> {
        let pkt = self.keyframe_buffer.pop().or_else(|| self.pending.take())?;
        let hdr = self.timeline.rewrite(&pkt);
        Some((hdr, pkt))
    }
}

struct SwitchingState<T> {
    pending: Option<T>,
    timeline: Timeline,
    keyframe_buffer: KeyframeBuffer<T>,

    new_ssrc: Ssrc,
    old_ssrc: Ssrc,
}

impl<T: Packet> SwitchingState<T> {
    fn process(mut self, packet: T) -> SequencerState<T> {
        assert!(self.new_ssrc != self.old_ssrc);

        if packet.ssrc() == self.new_ssrc {
            if self.keyframe_buffer.push(packet) {
                self.timeline.mark_need_rebase();
                return SequencerState::Stable(StableState {
                    pending: None,
                    active_ssrc: self.new_ssrc,
                    keyframe_buffer: self.keyframe_buffer,
                    timeline: self.timeline,
                });
            }
        } else {
            self.pending.replace(packet);
        }

        SequencerState::Switching(self)
    }

    fn poll(&mut self) -> Option<(TimingHeader, T)> {
        let pkt = self.pending.take()?;
        let hdr = self.timeline.rewrite(&pkt);
        Some((hdr, pkt))
    }
}

struct Timeline {
    frequency: Frequency,
    highest_seq_no: SeqNo,
    highest_rtp_ts: MediaTime,
    offset_seq_no: u64,
    offset_rtp_ts: u64,
    need_rebase: bool,
    last_marker: bool,
}

impl Timeline {
    fn new(frequency: Frequency) -> Self {
        let base_ts: u32 = rand::random();
        Self {
            frequency,
            highest_seq_no: SeqNo::new(),
            highest_rtp_ts: MediaTime::new(base_ts.into(), frequency),

            offset_seq_no: 0,
            offset_rtp_ts: 0,
            need_rebase: true,
            last_marker: false,
        }
    }

    // packet is guaranteed to be the first packet after a marker and is a keyframe
    fn rebase(&mut self, packet: &impl Packet) {
        debug_assert!(packet.is_keyframe_start());

        self.offset_seq_no = self.highest_seq_no.wrapping_sub(*packet.seq_no());
        self.offset_rtp_ts = self
            .highest_rtp_ts
            .numer()
            .wrapping_sub(packet.rtp_timestamp().rebase(self.frequency).numer());
    }

    fn mark_need_rebase(&mut self) {
        self.need_rebase = true;
    }

    fn rewrite(&mut self, packet: &impl Packet) -> TimingHeader {
        if self.need_rebase {
            self.rebase(packet);
        }

        let hdr = TimingHeader {
            seq_no: packet.seq_no().wrapping_add(self.offset_seq_no).into(),
            rtp_ts: MediaTime::new(
                packet
                    .rtp_timestamp()
                    .numer()
                    .wrapping_add(self.offset_rtp_ts),
                self.frequency,
            ),
            marker: packet.marker(),
            is_keyframe: packet.is_keyframe_start(),
            server_ts: packet.arrival_timestamp(),
        };

        if hdr.seq_no > self.highest_seq_no {
            self.highest_seq_no = hdr.seq_no;
        }

        if hdr.rtp_ts > self.highest_rtp_ts {
            self.highest_rtp_ts = hdr.rtp_ts;
        }

        hdr
    }

    fn on_frame_boundary(&mut self) -> bool {
        self.last_marker
    }
}

struct KeyframeBuffer<T> {
    buffer: BTreeMap<SeqNo, T>,
    current_ts: Option<MediaTime>,
    found_keyframe_start: bool,
}

impl<T: Packet> KeyframeBuffer<T> {
    fn new() -> Self {
        Self {
            buffer: BTreeMap::new(),
            current_ts: None,
            found_keyframe_start: false,
        }
    }

    fn clear(&mut self) {
        self.buffer.clear();
        self.current_ts = None;
        self.found_keyframe_start = false;
    }

    fn reset(&mut self, new_ts: MediaTime) {
        self.clear();
        self.current_ts = Some(new_ts);
    }

    fn push(&mut self, packet: T) -> bool {
        // TODO: check wrap around
        if let Some(current_ts) = self.current_ts {
            if packet.rtp_timestamp() < current_ts {
                tracing::warn!(
                    "late packet, dropping as ts < current_ts: {:?} < {:?}",
                    packet.rtp_timestamp(),
                    current_ts
                );
                return false;
            } else if packet.rtp_timestamp() > current_ts {
                self.reset(packet.rtp_timestamp());
            }
        } else {
            self.reset(packet.rtp_timestamp());
        }

        if packet.is_keyframe_start() {
            self.found_keyframe_start = true;
        }
        self.buffer.insert(packet.seq_no(), packet);
        self.found_keyframe_start
    }

    fn pop(&mut self) -> Option<T> {
        if !self.found_keyframe_start {
            return None;
        }

        let (_, packet) = self.buffer.pop_first()?;
        Some(packet)
    }
}

#[cfg(test)]
mod test {
    use crate::rtp::{Packet, PacketTiming, TimingHeader};
    use str0m::{
        media::{Frequency, MediaTime},
        rtp::{SeqNo, Ssrc},
    };
    use tokio::time::{Duration, Instant};

    /// A simple struct that implements the `Packet` trait for easy test case creation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TestPacket {
        pub header: TimingHeader,
        pub ssrc: Ssrc,
    }

    impl PacketTiming for TestPacket {
        fn seq_no(&self) -> SeqNo {
            self.header.seq_no
        }
        fn rtp_timestamp(&self) -> MediaTime {
            self.header.rtp_ts
        }
        fn arrival_timestamp(&self) -> Instant {
            self.header.server_ts
        }
    }

    impl Packet for TestPacket {
        fn marker(&self) -> bool {
            self.header.marker
        }
        fn is_keyframe_start(&self) -> bool {
            self.header.is_keyframe
        }
        fn ssrc(&self) -> Ssrc {
            self.ssrc
        }
    }

    /// Helper to create a `TestPacket` with specified properties.
    fn new_packet(
        ssrc: u32,
        seq_no: u64,
        rtp_ts: u64,
        arrival_offset_ms: u64,
        is_keyframe: bool,
        marker: bool,
    ) -> TestPacket {
        let now = Instant::now();
        TestPacket {
            ssrc: ssrc.into(),
            header: TimingHeader {
                seq_no: seq_no.into(),
                rtp_ts: MediaTime::new(rtp_ts, Frequency::NINETY_KHZ),
                server_ts: now + Duration::from_millis(arrival_offset_ms),
                is_keyframe,
                marker,
            },
        }
    }

    /// Scenario 1: A simple, well-ordered stream of two video frames.
    pub fn generate_simple_stream() -> Vec<TestPacket> {
        vec![
            // Frame 1 (Keyframe, timestamp 10000) - 3 packets
            new_packet(1, 100, 10000, 1, true, false), // Keyframe start
            new_packet(1, 101, 10000, 2, false, false),
            new_packet(1, 102, 10000, 3, false, true), // Marker bit
            // Frame 2 (Delta frame, timestamp 13000) - 2 packets
            new_packet(1, 103, 13000, 34, false, false),
            new_packet(1, 104, 13000, 35, false, true), // Marker bit
        ]
    }

    /// Scenario 2: A stream with a lost packet (sequence number 201 is missing).
    pub fn generate_stream_with_loss() -> Vec<TestPacket> {
        vec![
            // Frame 1 (Keyframe, timestamp 20000)
            new_packet(2, 200, 20000, 1, true, false), // Keyframe start
            // --- Packet with seq_no=201 is missing ---
            new_packet(2, 202, 20000, 3, false, true),
            // Frame 2 (Delta, timestamp 23000)
            new_packet(2, 203, 23000, 34, false, false),
            new_packet(2, 204, 23000, 35, false, true),
        ]
    }

    /// Scenario 3: A stream where packets for a frame arrive out of order.
    pub fn generate_stream_with_reordering() -> Vec<TestPacket> {
        vec![
            // Frame 1 (Keyframe, timestamp 30000) - packets arrive 300, 302, 301
            new_packet(3, 300, 30000, 1, true, false),
            new_packet(3, 302, 30000, 2, false, true), // Arrives early
            new_packet(3, 301, 30000, 3, false, false), // Arrives late
            // Frame 2 (Delta, timestamp 33000) - arrives correctly
            new_packet(3, 303, 33000, 34, false, false),
            new_packet(3, 304, 33000, 35, false, true),
        ]
    }

    /// Scenario 4: A simulcast layer switch from SSRC 10 to SSRC 20.
    pub fn generate_simulcast_switch_streams() -> (Vec<TestPacket>, Vec<TestPacket>) {
        // Stream from the low-resolution layer (SSRC 10)
        let old_stream = vec![
            new_packet(10, 1000, 50000, 1, true, false),
            new_packet(10, 1001, 50000, 2, false, true),
            new_packet(10, 1002, 53000, 34, false, true),
            // This packet might be sent while the switch is happening
            new_packet(10, 1003, 56000, 65, false, true),
        ];

        // Stream from the new, high-resolution layer (SSRC 20)
        let new_stream = vec![
            new_packet(20, 500, 80000, 66, true, false), // Must start with keyframe
            new_packet(20, 501, 80000, 67, false, false),
            new_packet(20, 502, 80000, 68, false, true),
            new_packet(20, 503, 83000, 100, false, true),
        ];

        (old_stream, new_stream)
    }

    /// Scenario 5: The keyframe-starting packet arrives after other packets for that frame.
    pub fn generate_late_keyframe_stream() -> Vec<TestPacket> {
        vec![
            // Packets for Frame 1 (timestamp 40000) arrive out of order
            new_packet(5, 401, 40000, 1, false, false), // Delta packet
            new_packet(5, 402, 40000, 2, false, true),  // Delta packet with marker
            new_packet(5, 400, 40000, 3, true, false),  // Keyframe start arrives last
            // Subsequent frame
            new_packet(5, 403, 43000, 34, false, true),
        ]
    }
}
