use std::collections::BTreeMap;
use std::marker::PhantomData;

use futures::Stream;
use str0m::media::{Frequency, MediaTime};
use str0m::rtp::{SeqNo, Ssrc};

use crate::rtp::{Packet, TimingHeader};

/// The standard 90kHz clock rate for video RTP, used for all internal timestamp math.
const VIDEO_FREQUENCY: Frequency = Frequency::NINETY_KHZ;
const AUDIO_FREQUENCY: Frequency = Frequency::FORTY_EIGHT_KHZ;

pub struct RtpSequencer<T> {
    switching: bool,

    last_stream: Option<StreamState>,
}

impl<T: Packet> RtpSequencer<T> {
    pub fn video() -> Self {
        Self::new(VIDEO_FREQUENCY)
    }

    pub fn audio() -> Self {
        Self::new(AUDIO_FREQUENCY)
    }

    pub fn new(frequency: Frequency) -> Self {
        let start_ts = rand::random();
        Self {
            frequency,
            forwarded_seq_no: SeqNo::new(),
            forwarded_rtp_ts: MediaTime::new(start_ts, frequency),
            switching: true,

            last_stream: None,
        }
    }

    pub fn push(&mut self, packet: &T) {
        match (self.last_stream, self.switching) {
            (None, _) => self.rebase(packet),
            (Some(last), false) if packet.ssrc() != last.ssrc => self.maybe_rebase(packet),
        }
    }

    pub fn pop(&mut self) -> Option<(SeqNo, MediaTime, T)> {
        todo!()
    }

    fn maybe_rebase(&mut self, packet: &T) {}
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
}

struct CommonState<T> {
    timeline: Timeline,
    keyframe_buffer: KeyframeBuffer<T>,
}

struct NewState<T> {
    common: CommonState<T>,
}

impl<T: Packet> NewState<T> {
    fn process(mut self, packet: T) -> SequencerState<T> {
        self.common.keyframe_buffer.push(packet);
        if let Some(packet) = self.common.keyframe_buffer.pop() {
            self.common.timeline.rebase(&packet);
            SequencerState::Stable(StableState {
                pending: None,
                common: self.common,
                active_ssrc: packet.ssrc(),
            })
        } else {
            SequencerState::New(self)
        }
    }
}

struct StableState<T> {
    pending: Option<T>,
    common: CommonState<T>,

    active_ssrc: Ssrc,
}

impl<T: Packet> StableState<T> {
    fn process(mut self, packet: T) -> SequencerState<T> {
        if packet.ssrc() != self.active_ssrc {
            SequencerState::Switching(SwitchingState {
                common: self.common,
                pending: self.pending,
                new_ssrc: packet.ssrc(),
                old_ssrc: self.active_ssrc,
            })
        } else {
            self.pending.replace(packet);
            SequencerState::Stable(self)
        }
    }

    fn poll(&mut self) -> Option<(TimingHeader, T)> {
        let pkt = self
            .common
            .keyframe_buffer
            .pop()
            .or_else(|| self.pending.take())?;
        let hdr = self.common.timeline.rewrite(&pkt);
        Some((hdr, pkt))
    }
}

struct SwitchingState<T> {
    pending: Option<T>,
    common: CommonState<T>,

    new_ssrc: Ssrc,
    old_ssrc: Ssrc,
}

impl<T: Packet> SwitchingState<T> {
    fn process(mut self, packet: T) -> SequencerState<T> {
        if packet.ssrc() == self.new_ssrc {
            let is_keyframe_start = self.common.keyframe_buffer.push(packet);
            SequencerState::Stable(())
        }
    }

    fn poll(&mut self) -> Option<(TimingHeader, T)> {
        let pkt = self
            .common
            .keyframe_buffer
            .pop()
            .or_else(|| self.pending.take())?;
        let hdr = self.common.timeline.rewrite(&pkt);
        Some((hdr, pkt))
    }
}

struct Timeline {
    frequency: Frequency,
    highest_seq_no: SeqNo,
    highest_rtp_ts: MediaTime,
    offset_seq_no: u64,
    offset_rtp_ts: u64,
}

impl Timeline {
    // packet is guaranteed to be the first packet after a marker and is a keyframe
    fn rebase(&mut self, packet: &impl Packet) {
        debug_assert!(packet.is_keyframe_start());

        self.offset_seq_no = self.highest_seq_no.wrapping_sub(*packet.seq_no());
        self.offset_rtp_ts = self
            .highest_rtp_ts
            .numer()
            .wrapping_sub(packet.rtp_timestamp().rebase(self.frequency).numer());
    }

    fn rewrite(&mut self, packet: &impl Packet) -> TimingHeader {
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

    fn reset(&mut self, new_ts: MediaTime) {
        self.buffer.clear();
        self.current_ts = Some(new_ts);
        self.found_keyframe_start = false;
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
