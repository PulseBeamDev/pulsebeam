//! Depacketize ordered encoded RTP. Jitter buffering and recovery are optional.

use std::{ops::Range, time::Instant};

use pulsebeam_core::{
    dd::{
        DdReadError, DependencyDescriptor, DependencyDescriptorReader, MandatoryFields,
        read_mandatory,
    },
    h264::{self, DepacketizationError, NalFlags},
    rtp_serial::extend_u32,
};

use super::jitter::GapReason;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Opus,
    H264,
}

impl Codec {
    pub const fn clock_rate(self) -> u32 {
        match self {
            Self::Opus => 48_000,
            Self::H264 => 90_000,
        }
    }
}

/// A sender-report anchor in this output RTP clock. NTP is a sender coordinate,
/// not a claim of synchronization with another sender or the receiver clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SenderReport {
    pub rtp_timestamp: u32,
    pub ntp: u64,
}

impl SenderReport {
    pub(crate) fn is_after(self, previous: Self) -> bool {
        let forward = self.ntp.wrapping_sub(previous.ntp);
        forward != 0 && forward < 1 << 63
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SenderTime {
    Unknown,
    Report { rtp_anchor: u64, ntp: u64 },
}

#[derive(Debug)]
pub enum Dependency {
    Absent,
    Parsed(Box<DependencyDescriptor>),
    MissingContext(MandatoryFields),
    Invalid(DdReadError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Audio,
    IndependentVideo,
    DependentVideo,
}

#[derive(Debug)]
pub struct Unit<S> {
    pub source: S,
    pub codec: Codec,
    pub first_sequence: u64,
    pub last_sequence: u64,
    /// Extended RTP time is presentation time, not the sequence/order key.
    pub rtp_time: Option<u64>,
    pub sender_time: SenderTime,
    pub arrived_at: Instant,
    pub access: Access,
    pub dependency: Dependency,
    pub data: Vec<u8>,
}

pub struct OrderedPacket<'a> {
    pub sequence: u64,
    pub timestamp: u32,
    pub rtp_time: Option<u64>,
    pub arrived_at: Instant,
    pub marker: bool,
    pub payload: &'a [u8],
    pub descriptor: Option<&'a [u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Gap(GapReason),
    MissingBoundary,
    InvalidVideo(DepacketizationError),
    EmptyOpus,
}

#[derive(Debug)]
pub struct Discontinuity<S> {
    pub source: S,
    pub sequence: Range<u64>,
    pub cause: Cause,
}

#[derive(Debug)]
pub enum Event<S> {
    Unit(Unit<S>),
    Discontinuity(Discontinuity<S>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    Retired,
    /// Supply the missing interval with `gap` before pushing this packet.
    OutOfOrder {
        expected: u64,
    },
}

struct Frame {
    first_sequence: u64,
    timestamp: u32,
    arrived_at: Instant,
    dependency: Dependency,
    flags: NalFlags,
}

struct Video {
    codec: h264::Depacketizer,
    frame: Option<Frame>,
    /// If the next frame boundary is unknown, discard through its marker,
    /// unless the next packet carries a qualified start indication.
    seeking: bool,
    discarded_timestamp: Option<u32>,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            codec: h264::Depacketizer::new(),
            frame: None,
            seeking: true,
            discarded_timestamp: None,
        }
    }
}

impl Video {
    fn lose(&mut self) {
        self.codec.reset();
        self.frame = None;
        self.discarded_timestamp = None;
        self.seeking = true;
    }
}

/// One codec and a single non-reused source lifetime. Reset by constructing a
/// new instance on source replacement; no old RTP or report can cross into it.
pub struct Depacketizer<S> {
    source: S,
    codec: Codec,
    video: Option<Video>,
    sequence: Option<u64>,
    dd: DependencyDescriptorReader,
    report: Option<SenderReport>,
}

impl<S: Copy + Eq> Depacketizer<S> {
    pub fn new(source: S, codec: Codec) -> Self {
        Self {
            source,
            codec,
            video: (codec == Codec::H264).then(Video::default),
            sequence: None,
            dd: DependencyDescriptorReader::new(),
            report: None,
        }
    }

    /// Apply a tighter video assembly bound before supplying any input.
    #[must_use]
    pub fn with_video_limit(mut self, bytes: std::num::NonZeroUsize) -> Self {
        if let Some(video) = &mut self.video {
            *video = Video {
                codec: h264::Depacketizer::with_max_access_unit_size(
                    bytes.get().min(h264::MAX_ACCESS_UNIT_SIZE),
                ),
                ..Video::default()
            };
        }
        self
    }

    /// Stop projecting sender time when the caller's report has expired.
    pub fn clear_report(&mut self) {
        self.report = None;
    }

    /// Only supply reports from the authenticated owner of the bound RTP
    /// clock. Zero, duplicate and regressing NTP coordinates are not anchors.
    pub fn observe_report(&mut self, source: S, report: SenderReport) -> bool {
        if source != self.source
            || report.ntp == 0
            || self.report.is_some_and(|current| !report.is_after(current))
        {
            return false;
        }
        self.report = Some(report);
        true
    }

    pub fn gap(
        &mut self,
        source: S,
        gap: Range<u64>,
        reason: GapReason,
    ) -> Result<Discontinuity<S>, Rejected> {
        if source != self.source {
            return Err(Rejected::Retired);
        }
        let expected = self.sequence.unwrap_or(gap.start);
        if gap.start != expected || gap.is_empty() {
            return Err(Rejected::OutOfOrder { expected });
        }
        self.sequence = Some(gap.end);
        self.dd.reset();
        if let Some(video) = &mut self.video {
            video.lose();
        }
        Ok(Discontinuity {
            source,
            sequence: gap,
            cause: Cause::Gap(reason),
        })
    }

    /// Push one packet in source sequence order. A real gap is a separate
    /// input; it cannot be hidden by a presentation timestamp change.
    pub fn push(
        &mut self,
        source: S,
        packet: OrderedPacket<'_>,
    ) -> Result<Option<Event<S>>, Rejected> {
        if source != self.source {
            return Err(Rejected::Retired);
        }
        let expected = self.sequence.unwrap_or(packet.sequence);
        if packet.sequence != expected {
            return Err(Rejected::OutOfOrder { expected });
        }
        self.sequence = Some(packet.sequence.saturating_add(1));
        let sender_time =
            match (packet.rtp_time, self.report) {
                (Some(timestamp), Some(report)) => extend_u32(report.rtp_timestamp, timestamp)
                    .map_or(SenderTime::Unknown, |rtp_anchor| SenderTime::Report {
                        rtp_anchor,
                        ntp: report.ntp,
                    }),
                _ => SenderTime::Unknown,
            };
        let dependency = read_dependency(&mut self.dd, packet.descriptor);
        let unit = match &mut self.video {
            Some(video) => match video.push(&packet, dependency) {
                Ok(Some(parts)) => parts,
                Ok(None) => return Ok(None),
                Err((cause, first_sequence)) => {
                    return Ok(Some(Event::Discontinuity(Discontinuity {
                        source,
                        sequence: first_sequence..packet.sequence.saturating_add(1),
                        cause,
                    })));
                }
            },
            None if packet.payload.is_empty() => {
                return Ok(Some(Event::Discontinuity(Discontinuity {
                    source,
                    sequence: packet.sequence..packet.sequence.saturating_add(1),
                    cause: Cause::EmptyOpus,
                })));
            }
            None => Parts {
                first_sequence: packet.sequence,
                arrived_at: packet.arrived_at,
                access: Access::Audio,
                dependency,
                data: packet.payload.to_vec(),
            },
        };
        Ok(Some(Event::Unit(Unit {
            source,
            codec: self.codec,
            first_sequence: unit.first_sequence,
            last_sequence: packet.sequence,
            rtp_time: packet.rtp_time,
            sender_time,
            arrived_at: unit.arrived_at,
            access: unit.access,
            dependency: unit.dependency,
            data: unit.data,
        })))
    }
}

struct Parts {
    first_sequence: u64,
    arrived_at: Instant,
    access: Access,
    dependency: Dependency,
    data: Vec<u8>,
}

impl Video {
    fn push(
        &mut self,
        packet: &OrderedPacket<'_>,
        dependency: Dependency,
    ) -> Result<Option<Parts>, (Cause, u64)> {
        let flags = h264::classify(packet.payload);
        let declared_start = match &dependency {
            Dependency::Parsed(dd) => dd.start_of_frame,
            Dependency::MissingContext(mandatory) => mandatory.start_of_frame,
            _ => false,
        };
        if let Some(frame) = &self.frame
            && (frame.timestamp != packet.timestamp || declared_start)
        {
            let first = frame.first_sequence;
            self.lose();
            self.discarded_timestamp = Some(packet.timestamp);
            self.seeking = !packet.marker;
            return Err((Cause::MissingBoundary, first));
        }
        if self.seeking {
            if declared_start
                || flags.sps()
                || self
                    .discarded_timestamp
                    .is_some_and(|old| old != packet.timestamp)
            {
                self.seeking = false;
            } else {
                self.seeking = !packet.marker;
                self.discarded_timestamp = Some(packet.timestamp);
                return Err((Cause::MissingBoundary, packet.sequence));
            }
        }
        let start = self.frame.is_none();
        let frame = self.frame.get_or_insert(Frame {
            first_sequence: packet.sequence,
            timestamp: packet.timestamp,
            arrived_at: packet.arrived_at,
            dependency,
            flags: NalFlags::empty(),
        });
        frame.arrived_at = frame.arrived_at.min(packet.arrived_at);
        frame.flags = NalFlags::from_components(
            frame.flags.sps() || flags.sps(),
            frame.flags.pps() || flags.pps(),
            frame.flags.idr() || flags.idr(),
        );
        match self.codec.push(packet.payload, start, packet.marker) {
            Ok(None) => Ok(None),
            Ok(Some(data)) => {
                let Some(frame) = self.frame.take() else {
                    return Ok(None);
                };
                let access = if frame.flags.sps() && frame.flags.pps() && frame.flags.idr() {
                    Access::IndependentVideo
                } else {
                    Access::DependentVideo
                };
                Ok(Some(Parts {
                    first_sequence: frame.first_sequence,
                    arrived_at: frame.arrived_at,
                    access,
                    dependency: frame.dependency,
                    data,
                }))
            }
            Err(error) => {
                let first = frame.first_sequence;
                self.lose();
                self.seeking = !packet.marker;
                self.discarded_timestamp = Some(packet.timestamp);
                Err((Cause::InvalidVideo(error), first))
            }
        }
    }
}

fn read_dependency(reader: &mut DependencyDescriptorReader, bytes: Option<&[u8]>) -> Dependency {
    let Some(bytes) = bytes else {
        return Dependency::Absent;
    };
    match reader.read(bytes) {
        Ok(dd) => Dependency::Parsed(Box::new(dd)),
        Err(DdReadError::NoStructure | DdReadError::UnknownTemplateId(_)) => {
            read_mandatory(bytes).map_or_else(Dependency::Invalid, Dependency::MissingContext)
        }
        Err(error) => Dependency::Invalid(error),
    }
}

/// Optional conservative video recovery. Complete dependent units remain
/// visible to the standalone depacketizer but are withheld by this stage.
pub struct VideoRecovery<S> {
    source: S,
    awaiting_independent: bool,
    refresh_taken: bool,
}

#[derive(Debug)]
pub enum Recovery<S> {
    Deliver(Event<S>),
    Withheld(Unit<S>),
    Retired,
}

impl<S: Copy + Eq> VideoRecovery<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            awaiting_independent: true,
            refresh_taken: false,
        }
    }

    pub fn apply(&mut self, event: Event<S>) -> Recovery<S> {
        match event {
            Event::Discontinuity(gap) if gap.source == self.source => {
                self.mark_loss();
                Recovery::Deliver(Event::Discontinuity(gap))
            }
            Event::Unit(unit) if unit.source == self.source => {
                if unit.access == Access::IndependentVideo {
                    self.awaiting_independent = false;
                } else if self.awaiting_independent && unit.access == Access::DependentVideo {
                    return Recovery::Withheld(unit);
                }
                Recovery::Deliver(Event::Unit(unit))
            }
            _ => Recovery::Retired,
        }
    }

    /// Fence dependent video after application-side loss, such as overflow.
    pub fn mark_loss(&mut self) {
        if !self.awaiting_independent {
            self.awaiting_independent = true;
            self.refresh_taken = false;
        }
    }

    /// Return one generation-scoped refresh request per recovery interval.
    /// Retain it on transport backpressure; no depacketizer sends feedback.
    pub fn take_refresh(&mut self) -> Option<S> {
        if !self.awaiting_independent || self.refresh_taken {
            return None;
        }
        self.refresh_taken = true;
        Some(self.source)
    }
}

#[cfg(test)]
mod tests;
