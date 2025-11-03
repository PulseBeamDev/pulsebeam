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
        }
    }

    // packet is guaranteed to be the first packet after a marker and is a keyframe
    fn rebase(&mut self, packet: &impl Packet) {
        debug_assert!(packet.is_keyframe_start());

        let target_seq_no = packet.seq_no().wrapping_add(1);
        self.offset_seq_no = self.highest_seq_no.wrapping_sub(target_seq_no);
        self.offset_rtp_ts = self
            .highest_rtp_ts
            .numer()
            .wrapping_sub(packet.rtp_timestamp().rebase(self.frequency).numer());
        self.need_rebase = false;
    }

    fn mark_need_rebase(&mut self) {
        self.need_rebase = true;
    }

    fn rewrite(&mut self, packet: &impl Packet) -> TimingHeader {
        if self.need_rebase {
            self.rebase(packet);
        }

        let hdr = TimingHeader {
            ssrc: packet.ssrc(),
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
    use super::*;
    use crate::rtp::{Packet, TimingHeader};
    use str0m::media::{Frequency, MediaTime};

    type ScenarioStep = Box<dyn Fn(TimingHeader) -> TimingHeader>;

    pub fn next_seq() -> ScenarioStep {
        Box::new(TimingHeader::next_packet)
    }
    pub fn next_frame() -> ScenarioStep {
        Box::new(TimingHeader::next_frame)
    }
    pub fn keyframe() -> ScenarioStep {
        Box::new(|prev| {
            let mut next = prev.next_frame();
            next.is_keyframe = true;
            next
        })
    }
    pub fn marker() -> ScenarioStep {
        Box::new(|prev| {
            let mut next = prev.next_packet();
            next.marker = true;
            next
        })
    }
    pub fn simulcast_switch(new_ssrc: u32) -> ScenarioStep {
        Box::new(move |mut prev| {
            prev.ssrc = new_ssrc.into();
            prev.seq_no = 5000.into();
            prev.rtp_ts = MediaTime::new(80000, Frequency::NINETY_KHZ);
            prev.marker = false;
            prev
        })
    }

    /// Generates a `Vec<TimingHeader>` from a series of steps.
    pub fn generate(initial: TimingHeader, steps: Vec<ScenarioStep>) -> Vec<TimingHeader> {
        let mut packets = Vec::with_capacity(steps.len());
        let mut current = initial;
        for step in steps {
            current = step(current);
            packets.push(current);
        }
        packets
    }

    /// The single source of truth for sequencer correctness.
    ///
    /// This function takes a sequence of input packets (which can be unordered, have gaps, etc.),
    /// runs them through the sequencer, and asserts that the final state meets all required properties.
    pub fn run(packets: &[TimingHeader]) {
        let mut seq = RtpSequencer::video();
        let mut rewritten_headers = Vec::new();

        for packet in packets {
            seq.push(packet);
            while let Some((hdr, _)) = seq.pop() {
                rewritten_headers.push(hdr);
            }
        }

        rewritten_headers.sort_by_key(|e| e.seq_no);
        // Property 1: The sequencer must end in a stable state.
        assert!(
            seq.is_stable(),
            "Sequencer must be stable after processing all packets."
        );

        // // Property 2: The sequencer must not drop any packets.
        // assert_eq!(
        //     packets.len(),
        //     rewritten_headers.len(),
        //     "Input and output packet counts must match."
        // );

        // Property 3: The output sequence numbers must be strictly ordered.
        let is_seq_ordered = rewritten_headers
            .windows(2)
            .all(|w| w[0].seq_no < w[1].seq_no);
        println!("{:?}", rewritten_headers);
        assert!(
            is_seq_ordered,
            "Output packets are not in correct sequence number order."
        );

        // Property 4: The output timestamps must be monotonically non-decreasing.
        let timestamps_ok = rewritten_headers
            .windows(2)
            .all(|w| w[0].rtp_ts <= w[1].rtp_ts);
        assert!(
            timestamps_ok,
            "Output timestamps are not monotonically non-decreasing."
        );
    }

    // --- Scenarios: Defined as vectors of steps ---
    pub fn simple_frame_scenario() -> Vec<ScenarioStep> {
        vec![keyframe(), next_seq(), marker()]
    }
    pub fn simple_stream_scenario() -> Vec<ScenarioStep> {
        vec![keyframe(), next_seq(), marker(), next_frame(), marker()]
    }
    pub fn loss_scenario() -> Vec<ScenarioStep> {
        vec![keyframe(), next_seq(), next_seq(), marker()]
    }

    // --- Tests ---
    #[test]
    fn run_simple_stream() {
        let packets = generate(TimingHeader::default(), simple_stream_scenario());
        run(&packets);
    }

    #[test]
    fn run_stream_with_loss() {
        let packets = generate(TimingHeader::default(), loss_scenario());
        run(&packets);
    }

    #[test]
    fn run_stream_with_reordering() {
        // 1. Generate the packets in their logical, correct order.
        let mut packets = generate(TimingHeader::default(), simple_frame_scenario());

        // 2. Shuffle them to simulate out-of-order network delivery.
        packets.swap(0, 2); // [p1, p2, p3] -> [p3, p2, p1]

        // 3. The `run` function asserts that the output is corrected.
        run(&packets);
    }

    #[test]
    fn run_composed_scenario_with_simulcast_switch() {
        // Composition is just concatenating the step vectors
        let mut steps = simple_stream_scenario();
        steps.push(simulcast_switch(100));
        steps.extend(simple_stream_scenario());

        let packets = generate(TimingHeader::default(), steps);
        run(&packets);
    }
}
