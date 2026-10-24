//! Owning public event subtype codecs. The SDK owns the wire format.
use crate::{BandwidthUsage, RtcEvent, RtcEventKind, Timestamp, ffi};
use std::{collections::BTreeMap, fmt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtcEventCodecError {
    UnsupportedEventKind,
    MismatchedEventKind,
    MismatchedOutputKind,
    TimestampOutOfRange,
}
impl fmt::Display for RtcEventCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedEventKind => "the pinned event class has no public codec",
            Self::MismatchedEventKind => "a native subtype encoder requires matching event types",
            Self::MismatchedOutputKind => "the parsed output does not match the native codec",
            Self::TimestampOutOfRange => "decoded native milliseconds would overflow microseconds",
        })
    }
}
impl std::error::Error for RtcEventCodecError {}

/// The native parse status, including its complete source-qualified message.
/// A false status may accompany records already appended by the native parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventLogParseStatus {
    ok: bool,
    message: String,
}
impl RtcEventLogParseStatus {
    fn from_native(value: ffi::FfiEventParseStatus) -> Self {
        Self {
            ok: value.ok,
            message: value.message,
        }
    }
    pub fn success() -> Self {
        Self::from_native(ffi::event_codec_status(true, "", "", 0))
    }
    pub fn error(error: &str, file: &str, line: i32) -> Self {
        Self::from_native(ffi::event_codec_status(false, error, file, line))
    }
    pub fn ok(&self) -> bool {
        self.ok
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}
impl fmt::Display for RtcEventLogParseStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggedAlrStateEvent {
    pub timestamp: Timestamp,
    pub in_alr: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggedAudioPlayoutEvent {
    pub timestamp: Timestamp,
    pub ssrc: u32,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggedBweDelayBasedUpdate {
    pub timestamp: Timestamp,
    pub bitrate_bps: i32,
    pub detector_state: BandwidthUsage,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggedStartEvent {
    pub timestamp: Timestamp,
    pub utc_start_time: Timestamp,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggedStopEvent {
    pub timestamp: Timestamp,
}

macro_rules! logged_time {
    ($($record:ty),+ $(,)?) => { $(
        impl $record {
            pub fn log_time(&self) -> Timestamp { self.timestamp }
            pub fn log_time_us(&self) -> Result<i64, crate::TimeValueError> { self.timestamp.microseconds() }
            pub fn log_time_ms(&self) -> Result<i64, crate::TimeValueError> { self.timestamp.millis() }
        }
    )+ };
}
logged_time!(
    LoggedAlrStateEvent,
    LoggedAudioPlayoutEvent,
    LoggedBweDelayBasedUpdate,
    LoggedStartEvent,
    LoggedStopEvent
);
impl LoggedStartEvent {
    pub fn utc_time(&self) -> Timestamp {
        self.utc_start_time
    }
}

/// Caller-owned vectors/map for the implemented native subtype parsers.
/// Native stub codecs never append records and retain their actual SDK error.
/// Public fields also allow an existing output prefix, just like the SDK's
/// in/out containers. Each parse appends, including partial records on failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RtcEventParsedRecords {
    AlrState(Vec<LoggedAlrStateEvent>),
    AudioPlayout(BTreeMap<u32, Vec<LoggedAudioPlayoutEvent>>),
    BweUpdateDelayBased(Vec<LoggedBweDelayBasedUpdate>),
    BeginLog(Vec<LoggedStartEvent>),
    EndLog(Vec<LoggedStopEvent>),
    NativeUnimplemented(RtcEventKind),
}
impl RtcEventParsedRecords {
    pub fn for_kind(kind: RtcEventKind) -> Result<Self, RtcEventCodecError> {
        Ok(match kind_code(kind) {
            0 => Self::AlrState(Vec::new()),
            4 => Self::AudioPlayout(BTreeMap::new()),
            7 => Self::BweUpdateDelayBased(Vec::new()),
            0x2501580 => Self::BeginLog(Vec::new()),
            0x2501581 => Self::EndLog(Vec::new()),
            1..=3 | 5..=6 | 8..=23 => Self::NativeUnimplemented(kind),
            _ => return Err(RtcEventCodecError::UnsupportedEventKind),
        })
    }
    pub fn kind(&self) -> RtcEventKind {
        match self {
            Self::AlrState(_) => RtcEventKind::AlrState,
            Self::AudioPlayout(_) => RtcEventKind::AudioPlayout,
            Self::BweUpdateDelayBased(_) => RtcEventKind::BweUpdateDelayBased,
            Self::BeginLog(_) => RtcEventKind::BeginLog,
            Self::EndLog(_) => RtcEventKind::EndLog,
            Self::NativeUnimplemented(kind) => *kind,
        }
    }
    /// Parse the subtype payload, excluding the outer encoded event tag/length.
    /// `batched` is the native outer-tag flag, not an inferred byte property.
    /// The successful Rust Result means native parsing ran, not that it
    /// succeeded: inspect the returned native status. Native partial output is
    /// retained even when that status is false. Arithmetic safety errors do
    /// not call the subtype parser or modify this output.
    pub fn parse_payload(
        &mut self,
        bytes: &[u8],
        batched: bool,
    ) -> Result<RtcEventLogParseStatus, RtcEventCodecError> {
        if let Self::NativeUnimplemented(kind) = self {
            match kind_code(*kind) {
                1..=3 | 5..=6 | 8..=23 => (),
                _ => return Err(RtcEventCodecError::MismatchedOutputKind),
            }
        }
        let result = ffi::event_codec_parse(kind_code(self.kind()), bytes, batched);
        match result.guard {
            0 => (),
            1 => return Err(RtcEventCodecError::TimestampOutOfRange),
            _ => return Err(RtcEventCodecError::UnsupportedEventKind),
        }
        for row in result.rows {
            let timestamp = Timestamp::from_native_microsecond_bits(row.timestamp);
            match self {
                Self::AlrState(out) => out.push(LoggedAlrStateEvent {
                    timestamp,
                    in_alr: row.in_alr,
                }),
                Self::AudioPlayout(out) => {
                    out.entry(row.ssrc)
                        .or_default()
                        .push(LoggedAudioPlayoutEvent {
                            timestamp,
                            ssrc: row.ssrc,
                        })
                }
                Self::BweUpdateDelayBased(out) => out.push(LoggedBweDelayBasedUpdate {
                    timestamp,
                    bitrate_bps: row.bitrate_bps,
                    detector_state: row.detector_state.into(),
                }),
                Self::BeginLog(out) => out.push(LoggedStartEvent {
                    timestamp,
                    utc_start_time: Timestamp::from_native_microsecond_bits(row.utc_time),
                }),
                Self::EndLog(out) => out.push(LoggedStopEvent { timestamp }),
                Self::NativeUnimplemented(_) => {
                    unreachable!("native stub codecs append no records")
                }
            }
        }
        Ok(RtcEventLogParseStatus::from_native(result.status))
    }
}
impl RtcEvent {
    /// Invoke the selected concrete native Encode, returning its complete binary
    /// event tag/length/payload. Empty batches and SDK stub encoders return their
    /// native empty byte sequence. Input loans end before this function returns.
    pub fn encode_batch(
        kind: RtcEventKind,
        events: &[&RtcEvent],
    ) -> Result<Vec<u8>, RtcEventCodecError> {
        let mut batch = ffi::event_codec_encoder(kind_code(kind));
        if batch.is_null() {
            return Err(RtcEventCodecError::UnsupportedEventKind);
        }
        for event in events {
            if !ffi::event_codec_add(batch.pin_mut(), event.native()) {
                return Err(RtcEventCodecError::MismatchedEventKind);
            }
        }
        Ok(ffi::event_codec_encode(
            batch.as_ref().expect("owned native encoder"),
        ))
    }
}
fn kind_code(kind: RtcEventKind) -> u32 {
    match kind {
        RtcEventKind::AlrState => 0,
        RtcEventKind::RouteChange => 1,
        RtcEventKind::RemoteEstimate => 2,
        RtcEventKind::AudioNetworkAdaptation => 3,
        RtcEventKind::AudioPlayout => 4,
        RtcEventKind::AudioReceiveStreamConfig => 5,
        RtcEventKind::AudioSendStreamConfig => 6,
        RtcEventKind::BweUpdateDelayBased => 7,
        RtcEventKind::BweUpdateLossBased => 8,
        RtcEventKind::BweUpdateScream => 9,
        RtcEventKind::DtlsTransportState => 10,
        RtcEventKind::DtlsWritableState => 11,
        RtcEventKind::IceCandidatePairConfig => 12,
        RtcEventKind::IceCandidatePair => 13,
        RtcEventKind::ProbeClusterCreated => 14,
        RtcEventKind::ProbeResultFailure => 15,
        RtcEventKind::ProbeResultSuccess => 16,
        RtcEventKind::RtcpPacketIncoming => 17,
        RtcEventKind::RtcpPacketOutgoing => 18,
        RtcEventKind::RtpPacketIncoming => 19,
        RtcEventKind::RtpPacketOutgoing => 20,
        RtcEventKind::VideoReceiveStreamConfig => 21,
        RtcEventKind::VideoSendStreamConfig => 22,
        RtcEventKind::FrameDecoded => 23,
        RtcEventKind::NetEqSetMinimumDelay => 24,
        RtcEventKind::BeginLog => 0x2501580,
        RtcEventKind::EndLog => 0x2501581,
        RtcEventKind::Unknown(value) => value,
    }
}
