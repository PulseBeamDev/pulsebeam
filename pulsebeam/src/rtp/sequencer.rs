use std::collections::BTreeMap;
use std::fmt::{Debug, Display};
use str0m::media::{Frequency, MediaTime};
use str0m::rtp::{SeqNo, Ssrc};

use crate::rtp::{AUDIO_FREQUENCY, RtpPacket, VIDEO_FREQUENCY};

#[derive(Debug)]
pub struct RtpSequencer {
    state: SequencerState,
}

impl RtpSequencer {
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

    pub fn push(&mut self, packet: &RtpPacket) {
        let cloned = packet.clone();
        tracing::trace!("tracing:push={}", *packet.seq_no);
        let state_id = self.state.id();
        let new_state = match std::mem::replace(&mut self.state, SequencerState::Invalid) {
            SequencerState::New(state) => state.process(cloned),
            SequencerState::Stable(state) => state.process(cloned),
            SequencerState::Switching(state) => state.process(cloned),
            SequencerState::Invalid => unreachable!(),
        };
        if state_id != new_state.id() {
            tracing::info!("updated sequencer state to {}", new_state);
        }
        self.state = new_state;
    }

    pub fn pop(&mut self) -> Option<RtpPacket> {
        let item = match &mut self.state {
            SequencerState::Stable(state) => state.poll(),
            SequencerState::Switching(state) => state.poll(),
            _ => None,
        }?;
        tracing::trace!("tracing:pop={}", item.seq_no);
        Some(item)
    }
}

enum SequencerState {
    Invalid,
    New(NewState),
    Stable(StableState),
    Switching(SwitchingState),
}

impl Display for SequencerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => f.write_str("invalid"),
            Self::New(_) => f.write_str("new"),
            Self::Stable(_) => f.write_str("stable"),
            Self::Switching(_) => f.write_str("switching"),
        }
    }
}

impl Debug for SequencerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => f.write_str("invalid"),
            Self::New(_) => f.write_str("new"),
            Self::Stable(_) => f.write_str("stable"),
            Self::Switching(_) => f.write_str("switching"),
        }
    }
}

impl SequencerState {
    #[inline]
    fn id(&self) -> usize {
        match self {
            Self::Invalid => 0,
            Self::New(_) => 1,
            Self::Stable(_) => 2,
            Self::Switching(_) => 3,
        }
    }
}

struct NewState {
    timeline: Timeline,
    keyframe_buffer: KeyframeBuffer,
}

impl NewState {
    fn process(mut self, packet: RtpPacket) -> SequencerState {
        let new_ssrc = packet.raw_header.ssrc;
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

struct StableState {
    pending: Option<RtpPacket>,
    timeline: Timeline,
    keyframe_buffer: KeyframeBuffer,

    active_ssrc: Ssrc,
}

impl StableState {
    fn process(mut self, packet: RtpPacket) -> SequencerState {
        if packet.raw_header.ssrc == self.active_ssrc {
            self.pending.replace(packet);
            SequencerState::Stable(self)
        } else {
            let new_ssrc = packet.raw_header.ssrc;
            self.keyframe_buffer.reset();
            self.keyframe_buffer.push(packet);
            SequencerState::Switching(SwitchingState {
                pending: self.pending,
                keyframe_buffer: self.keyframe_buffer,
                timeline: self.timeline,
                new_ssrc,
                old_ssrc: self.active_ssrc,
            })
        }
    }

    fn poll(&mut self) -> Option<RtpPacket> {
        let pkt = self.keyframe_buffer.pop().or_else(|| self.pending.take())?;
        let pkt = self.timeline.rewrite(pkt);
        Some(pkt)
    }
}

struct SwitchingState {
    pending: Option<RtpPacket>,
    timeline: Timeline,
    keyframe_buffer: KeyframeBuffer,

    new_ssrc: Ssrc,
    old_ssrc: Ssrc,
}

impl SwitchingState {
    fn process(mut self, packet: RtpPacket) -> SequencerState {
        assert!(self.new_ssrc != self.old_ssrc);

        if packet.raw_header.ssrc == self.new_ssrc {
            if self.keyframe_buffer.push(packet) {
                self.timeline.mark_need_rebase();
                return SequencerState::Stable(StableState {
                    pending: None,
                    active_ssrc: self.new_ssrc,
                    keyframe_buffer: self.keyframe_buffer,
                    timeline: self.timeline,
                });
            }
        } else if packet.raw_header.ssrc == self.old_ssrc {
            self.pending.replace(packet);
        }

        SequencerState::Switching(self)
    }

    fn poll(&mut self) -> Option<RtpPacket> {
        let pkt = self.pending.take()?;
        let pkt = self.timeline.rewrite(pkt);
        Some(pkt)
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
        let base_seq_no: u8 = rand::random();
        let base_ts: u32 = rand::random();
        Self {
            frequency,
            highest_seq_no: SeqNo::from(base_seq_no as u64),
            highest_rtp_ts: MediaTime::new(base_ts.into(), frequency),

            offset_seq_no: 0,
            offset_rtp_ts: 0,
            need_rebase: true,
        }
    }

    // packet is guaranteed to be the first packet after a marker and is a keyframe
    fn rebase(&mut self, packet: &RtpPacket) {
        debug_assert!(packet.is_keyframe_start);

        let target_seq_no = self.highest_seq_no.wrapping_add(1);
        self.offset_seq_no = target_seq_no.wrapping_sub(*packet.seq_no);
        self.offset_rtp_ts = self
            .highest_rtp_ts
            .numer()
            .wrapping_sub(packet.rtp_ts.rebase(self.frequency).numer());
        self.need_rebase = false;
    }

    fn mark_need_rebase(&mut self) {
        self.need_rebase = true;
    }

    fn rewrite(&mut self, mut pkt: RtpPacket) -> RtpPacket {
        if self.need_rebase {
            self.rebase(&pkt);
        }

        pkt.seq_no = pkt.seq_no.wrapping_add(self.offset_seq_no).into();
        pkt.rtp_ts = MediaTime::new(
            pkt.rtp_ts.numer().wrapping_add(self.offset_rtp_ts),
            self.frequency,
        );

        if pkt.seq_no > self.highest_seq_no {
            self.highest_seq_no = pkt.seq_no;
        }

        if pkt.rtp_ts > self.highest_rtp_ts {
            self.highest_rtp_ts = pkt.rtp_ts;
        }

        pkt
    }
}

/// A buffer for a single frame, corresponding to one RTP timestamp.
struct FrameBuffer {
    packets: BTreeMap<SeqNo, RtpPacket>,
    has_keyframe_start: bool,
    has_marker: bool,
}

impl FrameBuffer {
    fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            has_keyframe_start: false,
            has_marker: false,
        }
    }

    fn push(&mut self, packet: RtpPacket) {
        if packet.is_keyframe_start {
            self.has_keyframe_start = true;
        }
        if packet.raw_header.marker {
            self.has_marker = true;
        }
        self.packets.insert(packet.seq_no, packet);
    }

    fn is_complete(&self) -> bool {
        self.has_marker
    }

    fn is_complete_keyframe(&self) -> bool {
        self.has_keyframe_start && self.has_marker
    }
}

/// A buffer that can hold multiple frames, sorted by timestamp, and intelligently
/// decides which frame is ready to be emitted.
struct KeyframeBuffer {
    /// Buffers sorted by timestamp, allowing us to process frames in order.
    buffers: BTreeMap<MediaTime, FrameBuffer>,
    /// Tracks if we've successfully started the stream with a keyframe.
    has_emitted_keyframe: bool,
    /// The timestamp of the last packet popped from the buffer. This is the "high water mark"
    /// used to reject stale packets.
    last_popped_ts: Option<MediaTime>,
}

impl KeyframeBuffer {
    fn new() -> Self {
        Self {
            buffers: BTreeMap::new(),
            has_emitted_keyframe: false,
            last_popped_ts: None,
        }
    }

    fn clear(&mut self) {
        self.buffers.clear();
        self.has_emitted_keyframe = false;
        self.last_popped_ts = None;
    }

    fn reset(&mut self) {
        self.clear();
    }

    /// Pushes a packet into the correct frame buffer based on its timestamp.
    /// Returns true if, after this push, there is a frame ready to be popped.
    fn push(&mut self, packet: RtpPacket) -> bool {
        let pkt_ts = packet.rtp_ts;

        // **THE FIX**: Check against the high water mark. If this packet is from a timestamp
        // we have already moved past, drop it immediately.
        if let Some(last_ts) = self.last_popped_ts {
            if pkt_ts <= last_ts {
                tracing::warn!(
                    "Dropping stale packet for ts {:?} which is <= last popped ts {:?}",
                    pkt_ts,
                    last_ts
                );
                // Return readiness without adding the packet. The state doesn't change.
                return self.is_ready();
            }
        }

        self.buffers
            .entry(pkt_ts)
            .or_insert_with(FrameBuffer::new)
            .push(packet);

        self.is_ready()
    }

    /// Checks if there's a frame ready to be drained.
    fn is_ready(&self) -> bool {
        // Find the oldest, complete keyframe. This is our primary synchronization point.
        let oldest_complete_kf = self
            .buffers
            .iter()
            .find(|(_, frame)| frame.is_complete_keyframe());

        if oldest_complete_kf.is_some() {
            // If we have a complete keyframe, we are ready to start emitting.
            return true;
        }

        // If we've already started the stream, we can also emit complete P-frames.
        if self.has_emitted_keyframe {
            if let Some((_, frame)) = self.buffers.first_key_value() {
                // If the oldest frame we have is complete, we're ready.
                return frame.is_complete();
            }
        }

        false
    }

    /// Pops the next packet from the next ready frame.
    fn pop(&mut self) -> Option<RtpPacket> {
        let mut clear_before_ts = None;
        let mut pop_from_ts = None;

        // --- Decision Logic ---
        if let Some((ts, _)) = self
            .buffers
            .iter()
            .find(|(_, frame)| frame.is_complete_keyframe())
        {
            clear_before_ts = Some(*ts);
            pop_from_ts = Some(*ts);
        } else if self.has_emitted_keyframe {
            if let Some((ts, frame)) = self.buffers.first_key_value() {
                if frame.is_complete() {
                    pop_from_ts = Some(*ts);
                }
            }
        }

        // --- Execution Logic ---
        if let Some(ts) = clear_before_ts {
            self.buffers.retain(|&k, _| k >= ts);
        }

        if let Some(ts) = pop_from_ts {
            let frame = self.buffers.get_mut(&ts).unwrap();
            if frame.has_keyframe_start {
                self.has_emitted_keyframe = true;
            }

            let (_, packet) = frame.packets.pop_first()?;

            self.last_popped_ts = Some(packet.rtp_ts);

            if frame.packets.is_empty() {
                self.buffers.remove(&ts);
            }
            return Some(packet);
        }

        None
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::test_utils::*;
    use str0m::media::{Frequency, MediaTime};

    fn run_with(mut seq: RtpSequencer, packets: &[RtpPacket]) {
        let mut rewritten_headers = Vec::new();

        // Inputs must not have duplications. Deduplication is already handled at this layer.
        println!("input:\n");
        print_packets(packets);

        for packet in packets {
            seq.push(packet);
            while let Some(pkt) = seq.pop() {
                rewritten_headers.push(pkt);
            }
        }

        rewritten_headers.sort_by_key(|e| e.seq_no);

        println!("\noutput:\n");
        print_packets(&rewritten_headers);

        // === PROPERTY 1: Final state must be stable ===
        assert!(
            seq.is_stable(),
            "Sequencer must end in stable state, stuck at {}",
            seq.state
        );

        // === PROPERTY 2: Output seqno must be *strictly increasing* and *gap-free* ===
        let seq_nos: Vec<u64> = rewritten_headers.iter().map(|h| (*h.seq_no)).collect();
        let is_continuous = seq_nos
            .iter()
            .zip(seq_nos.iter().skip(1))
            .all(|(a, b)| *b == a.wrapping_add(1));
        assert!(
            is_continuous,
            "Output sequence numbers must be contiguous (no gaps)"
        );

        // === PROPERTY 3: RTP timestamps non-decreasing ===
        let ts_ok = rewritten_headers
            .windows(2)
            .all(|w| w[0].rtp_ts <= w[1].rtp_ts);
        assert!(ts_ok, "RTP timestamps must be non-decreasing");

        // === PROPERTY 4: Only drop packets in allowed cases ===
        // Allowed drops:
        // - Non-keyframe packets before first keyframe
        // - Packets from old layer after switch, if not pending
        // - Late packets (ts < current_ts in keyframe buffer)
        //
        // But: never drop a packet that arrived after keyframe and belongs to active layer

        // === PROPERTY 5: Keyframe integrity ===
        // Every keyframe must be complete: starts with K, ends with M, all in between present
        let mut i = 0;
        while i < rewritten_headers.len() {
            let h = &rewritten_headers[i];
            if h.is_keyframe_start {
                let kf_start = i;
                let mut kf_end = i;
                let mut has_marker = false;

                while kf_end < rewritten_headers.len() {
                    let end_h = &rewritten_headers[kf_end];
                    if end_h.rtp_ts != h.rtp_ts {
                        break;
                    }
                    if end_h.raw_header.marker {
                        has_marker = true;
                        kf_end += 1;
                        break;
                    }
                    kf_end += 1;
                }

                assert!(has_marker, "Keyframe at idx {} has no marker bit", kf_start);
                // All packets in keyframe must be present (seqno contiguous)
                let kf_seqs: Vec<u64> = rewritten_headers[kf_start..kf_end]
                    .iter()
                    .map(|h| (*h.seq_no))
                    .collect();
                let kf_continuous = kf_seqs
                    .iter()
                    .zip(kf_seqs.iter().skip(1))
                    .all(|(a, b)| *b == a.wrapping_add(1));
                assert!(kf_continuous, "Keyframe packets not contiguous");

                i = kf_end;
            } else {
                i += 1;
            }
        }

        // === PROPERTY 6: SSRC switch behavior ===
        // - Old SSRC packets must be drained before new SSRC emits
        // - After switch, no old SSRC packets allowed
        for window in rewritten_headers.windows(3) {
            let [a, b, c] = window else {
                continue;
            };

            if b.raw_header.ssrc != c.raw_header.ssrc {
                assert_ne!(a, c, "Multiple SSRC switches not allowed in test");
            }
        }
    }

    pub fn run(packets: &[RtpPacket]) {
        run_with(RtpSequencer::video(), packets);
    }

    pub fn print_packets(packets: &[RtpPacket]) {
        use std::cmp::max;

        println!("\n--- Packet Inspector ({} packets) ---", packets.len());

        if packets.is_empty() {
            println!("(No packets to display)");
            println!("{}", "-".repeat(80));
            return;
        }

        // Determine max width for each column dynamically
        let mut max_idx = 3;
        let mut max_ssrc = 4; // "SSRC"
        let mut max_seq = 6; // "Seq No"
        let mut max_dseq = 6; // "Δ Seq"
        let mut max_ts = 9; // "Timestamp"
        let mut max_dts = 6; // "Δ TS"

        let mut rows = Vec::new();

        for (i, current_header) in packets.iter().enumerate() {
            let (delta_seq_str, delta_ts_str) = if i > 0 {
                let prev_header = &packets[i - 1];
                let delta_seq = current_header.seq_no.wrapping_sub(*prev_header.seq_no);
                let delta_ts = current_header
                    .rtp_ts
                    .numer()
                    .wrapping_sub(prev_header.rtp_ts.numer());
                (
                    format!("{:+}", delta_seq as i64),
                    format!("{:+}", delta_ts as i64),
                )
            } else {
                ("(base)".to_string(), "(base)".to_string())
            };

            let mut flags = String::new();
            if current_header.is_keyframe_start {
                flags.push('K');
            }
            if current_header.raw_header.marker {
                flags.push('M');
            }

            let ssrc = format!("{}", current_header.raw_header.ssrc);
            let seq = format!("{}", current_header.seq_no);
            let ts = format!("{}", current_header.rtp_ts.numer());

            // Track column width
            max_idx = max(max_idx, i.to_string().len());
            max_ssrc = max(max_ssrc, ssrc.len());
            max_seq = max(max_seq, seq.len());
            max_dseq = max(max_dseq, delta_seq_str.len());
            max_ts = max(max_ts, ts.len());
            max_dts = max(max_dts, delta_ts_str.len());

            rows.push((i, ssrc, seq, delta_seq_str, ts, delta_ts_str, flags));
        }

        // Print header
        println!(
            "{:<width_idx$} | {:<width_ssrc$} | {:<width_seq$} | {:<width_dseq$} | {:<width_ts$} | {:<width_dts$} | {}",
            "Idx",
            "SSRC",
            "Seq No",
            "Δ Seq",
            "Timestamp",
            "Δ TS",
            "Flags",
            width_idx = max_idx,
            width_ssrc = max_ssrc,
            width_seq = max_seq,
            width_dseq = max_dseq,
            width_ts = max_ts,
            width_dts = max_dts,
        );

        let total_width = max_idx + max_ssrc + max_seq + max_dseq + max_ts + max_dts + 6 * 3 + 7; // spacing & dividers
        println!("{}", "-".repeat(total_width));

        // Print rows
        for (i, ssrc, seq, dseq, ts, dts, flags) in rows {
            println!(
                "{:<width_idx$} | {:<width_ssrc$} | {:<width_seq$} | {:<width_dseq$} | {:<width_ts$} | {:<width_dts$} | {}",
                i,
                ssrc,
                seq,
                dseq,
                ts,
                dts,
                flags,
                width_idx = max_idx,
                width_ssrc = max_ssrc,
                width_seq = max_seq,
                width_dseq = max_dseq,
                width_ts = max_ts,
                width_dts = max_dts,
            );
        }

        println!("{}", "-".repeat(total_width));
    }

    // --- Scenarios: Defined as vectors of steps ---
    pub fn simple_frame() -> Vec<ScenarioStep> {
        vec![keyframe(), next_seq(), marker()]
    }
    pub fn simple_stream() -> Vec<ScenarioStep> {
        vec![keyframe(), next_seq(), marker(), next_frame(), marker()]
    }
    pub fn loss_scenario() -> Vec<ScenarioStep> {
        vec![keyframe(), next_seq(), next_seq(), marker()]
    }

    // --- Tests ---
    #[test]
    fn run_simple_stream() {
        let packets = generate(RtpPacket::default(), simple_stream());
        run(&packets);
    }

    #[test]
    fn run_stream_with_reordering() {
        let mut packets = generate(RtpPacket::default(), simple_frame());
        packets.swap(0, 2); // [M, p1, K] → should still work
        run(&packets);
    }

    #[test]
    fn simulcast_switch_mid_keyframe() {
        let mut steps = vec![keyframe(), next_seq()]; // partial keyframe
        steps.push(simulcast_switch(100, 5000, 80000));
        steps.extend(vec![keyframe(), next_seq(), marker()]);
        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
        // Expect: old partial keyframe dropped
    }

    #[test]
    fn late_packet_from_old_layer() {
        let mut steps = simple_stream();
        steps.push(simulcast_switch(100, 5000, 80000));
        steps.extend(simple_stream());

        // TODO: harden against old packets
        let packets = generate(RtpPacket::default(), steps);
        // let old_ssrc = packets[0].ssrc;
        // let mut late = packets[1];
        // late.ssrc = old_ssrc;
        // packets.push(late); // late old-layer packet

        run(&packets);
        // Expect: late packet dropped
    }

    #[test]
    fn multiple_simulcast_switches() {
        let mut steps = simple_frame();
        steps.push(simulcast_switch(100, 5000, 80000));
        steps.extend(simple_frame());
        steps.push(simulcast_switch(200, 10000, 160000));
        steps.extend(simple_frame());
        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
    }

    #[test]
    fn rtp_timestamp_wraparound() {
        let initial = RtpPacket {
            rtp_ts: MediaTime::new((u32::MAX - 100) as u64, Frequency::NINETY_KHZ),
            ..Default::default()
        };
        let steps = vec![keyframe(), next_seq(), next_seq(), marker()];
        let packets = generate(initial, steps);
        run(&packets);
    }

    #[test]
    fn sequence_number_wraparound() {
        let initial = RtpPacket {
            seq_no: SeqNo::from((u16::MAX - 5) as u64),
            ..Default::default()
        };
        let steps = vec![
            keyframe(),
            next_seq(),
            next_seq(),
            next_seq(),
            next_seq(),
            marker(),
        ];
        let packets = generate(initial, steps);
        run(&packets);
    }

    #[test]
    fn loss_before_keyframe() {
        let steps = vec![next_seq(), next_seq(), keyframe(), marker()];
        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
        // Expect: first two dropped
    }

    #[test]
    fn high_packet_burst() {
        let mut steps = vec![keyframe()];
        steps.extend((0..1000).map(|_| next_seq()));
        steps.push(marker());
        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
    }

    #[test]
    fn audio_sequencer_basic() {
        let steps = vec![keyframe(), next_seq(), marker()];
        let packets = generate(RtpPacket::default(), steps);
        let seq = RtpSequencer::audio();

        run_with(seq, &packets);
    }

    #[test]
    fn out_of_order_keyframe_fragments() {
        let ordered = vec![keyframe(), next_seq(), next_seq(), marker()];
        let mut packets = generate(RtpPacket::default(), ordered);
        // Scramble order
        packets = vec![
            packets[2].clone(),
            packets[0].clone(),
            packets[3].clone(),
            packets[1].clone(),
        ];
        run(&packets);
    }

    #[test]
    fn high_jitter_reordering() {
        // Simulates a chaotic network where packets are significantly reordered.
        let ordered_stream = vec![
            keyframe(), // KF1 starts
            next_seq(),
            marker(),     // KF1 ends
            next_frame(), // P-frame 1
            marker(),
            next_frame(), // KF2 starts
            next_seq(),
            marker(), // KF2 ends
        ];
        let packets = generate(RtpPacket::default(), ordered_stream);

        // Extreme reorder: [KF1-p1, KF2-p2, P1-M, KF1-K, KF2-M, P1-p1, KF2-K, KF1-M]
        let reordered_indices = [1, 6, 4, 0, 7, 3, 5, 2];
        let disordered_packets: Vec<_> = reordered_indices
            .iter()
            .map(|&i| packets[i].clone())
            .collect();

        run(&disordered_packets);
        // Expect: Sequencer should buffer and reorder everything correctly.
    }

    #[test]
    fn packet_loss_and_recovery_with_new_keyframe() {
        // Simulates losing part of a keyframe, followed by a new keyframe which should allow recovery.
        let mut steps = vec![
            keyframe(),
            // next_seq(), // This packet is "lost"
            next_seq(),
            marker(),
        ];
        // Stream continues but should be un-decodable
        steps.extend(vec![next_frame(), marker()]);
        // A new keyframe arrives, allowing the stream to recover
        steps.extend(simple_frame());

        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
        // Expect: The first incomplete keyframe and the following P-frame are dropped.
        // The stream resumes perfectly from the second keyframe.
    }

    #[test]
    fn simulcast_switch_with_loss_of_new_keyframe_start() {
        let mut steps = simple_stream(); // Emit a valid frame on the old SSRC
        steps.push(simulcast_switch(555, 1000, 90000));
        steps.extend(vec![
            // keyframe(), // The crucial first packet of the new stream is "lost"
            next_seq(),
            marker(),
        ]);
        // Now send a complete, valid keyframe on the new SSRC
        steps.extend(simple_frame());

        let packets = generate(RtpPacket::default(), steps);
        run(&packets);
        // Expect: The old SSRC packet should be emitted. The partial new keyframe is dropped.
        // The sequencer recovers and emits the final complete keyframe.
    }

    #[test]
    #[ignore]
    fn stale_packets_from_previous_timestamp_arrive_late() {
        // Simulates a scenario where packets from an old, abandoned keyframe
        // arrive after a newer keyframe has already been processed.
        let mut initial_kf = generate(RtpPacket::default(), vec![keyframe(), next_seq()]);

        let mut next_kf_time = initial_kf[0].clone();
        next_kf_time.rtp_ts =
            MediaTime::new(initial_kf[0].rtp_ts.numer() + 3000, Frequency::NINETY_KHZ);

        let mut subsequent_kf = generate(next_kf_time, simple_frame());

        // Arrival order: [NewKF-p1, NewKF-p2, NewKF-M, OldKF-p1, OldKF-p2]
        let mut packets = Vec::new();
        packets.append(&mut subsequent_kf);
        packets.append(&mut initial_kf);

        run(&packets);
        // Expect: The stale, old keyframe packets are correctly identified as late and dropped.
    }
}
