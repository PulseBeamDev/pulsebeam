//! Owned deprecated native stats results. SDK Value refs never leave signaling.
use crate::{OperationId, PeerError, ffi};
use std::fmt;

/// SDK output level; Debug includes implementation-specific extra metrics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum LegacyStatsOutputLevel {
    Standard = 0,
    Debug = 1,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LegacyStatsRequestError {
    Binding(PeerError),
    NativeRejected,
}
impl From<PeerError> for LegacyStatsRequestError {
    fn from(value: PeerError) -> Self {
        Self::Binding(value)
    }
}
impl fmt::Display for LegacyStatsRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binding(e) => e.fmt(f),
            Self::NativeRejected => f.write_str(
                "native legacy GetStats rejected the request without scheduling a callback",
            ),
        }
    }
}
impl std::error::Error for LegacyStatsRequestError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(i32)]
pub enum LegacyStatsType {
    Session = 0,
    Transport = 1,
    Component = 2,
    CandidatePair = 3,
    Bwe = 4,
    Ssrc = 5,
    RemoteSsrc = 6,
    Track = 7,
    IceLocalCandidate = 8,
    IceRemoteCandidate = 9,
    Certificate = 10,
    DataChannel = 11,
}
impl LegacyStatsType {
    fn from_native(v: i32) -> Self {
        match v {
            0 => Self::Session,
            1 => Self::Transport,
            2 => Self::Component,
            3 => Self::CandidatePair,
            4 => Self::Bwe,
            5 => Self::Ssrc,
            6 => Self::RemoteSsrc,
            7 => Self::Track,
            8 => Self::IceLocalCandidate,
            9 => Self::IceRemoteCandidate,
            10 => Self::Certificate,
            11 => Self::DataChannel,
            _ => unreachable!("pinned legacy report type"),
        }
    }
}
/// Every pinned SDK metric name, including non-standard/deprecated entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(i32)]
pub enum LegacyStatsName {
    ActiveConnection = 0,
    AecDivergentFilterFraction = 1,
    AudioInputLevel = 2,
    AudioOutputLevel = 3,
    BytesReceived = 4,
    BytesSent = 5,
    CodecImplementationName = 6,
    ConcealedSamples = 7,
    ConcealmentEvents = 8,
    DataChannelId = 9,
    FramesDecoded = 10,
    FramesEncoded = 11,
    JitterBufferDelay = 12,
    MediaType = 13,
    PacketsLost = 14,
    PacketsReceived = 15,
    PacketsSent = 16,
    Protocol = 17,
    QpSum = 18,
    Receiving = 19,
    SelectedCandidatePairId = 20,
    Ssrc = 21,
    State = 22,
    TotalAudioEnergy = 23,
    TotalSamplesDuration = 24,
    TotalSamplesReceived = 25,
    TransportId = 26,
    SentPingRequestsTotal = 27,
    SentPingRequestsBeforeFirstResponse = 28,
    SentPingResponses = 29,
    RecvPingRequests = 30,
    RecvPingResponses = 31,
    SentStunKeepaliveRequests = 32,
    RecvStunKeepaliveResponses = 33,
    StunKeepaliveRttTotal = 34,
    StunKeepaliveRttSquaredTotal = 35,
    AccelerateRate = 36,
    ActualEncBitrate = 37,
    AdaptationChanges = 38,
    AvailableReceiveBandwidth = 39,
    AvailableSendBandwidth = 40,
    AvgEncodeMs = 41,
    BandwidthLimitedResolution = 42,
    BucketDelay = 43,
    CaptureStartNtpTimeMs = 44,
    CandidateIPAddress = 45,
    CandidateNetworkType = 46,
    CandidatePortNumber = 47,
    CandidatePriority = 48,
    CandidateTransportType = 49,
    CandidateType = 50,
    ChannelId = 51,
    CodecName = 52,
    Component = 53,
    ContentName = 54,
    ContentType = 55,
    CpuLimitedResolution = 56,
    CurrentDelayMs = 57,
    DecodeMs = 58,
    DecodingCNG = 59,
    DecodingCTN = 60,
    DecodingCTSG = 61,
    DecodingMutedOutput = 62,
    DecodingNormal = 63,
    DecodingPLC = 64,
    DecodingCodecPLC = 65,
    DecodingPLCCNG = 66,
    Der = 67,
    DtlsCipher = 68,
    EchoDelayMedian = 69,
    EchoDelayStdDev = 70,
    EchoReturnLoss = 71,
    EchoReturnLossEnhancement = 72,
    EncodeUsagePercent = 73,
    ExpandRate = 74,
    Fingerprint = 75,
    FingerprintAlgorithm = 76,
    FirsReceived = 77,
    FirsSent = 78,
    FirstFrameReceivedToDecodedMs = 79,
    FrameHeightInput = 80,
    FrameHeightReceived = 81,
    FrameHeightSent = 82,
    FrameRateDecoded = 83,
    FrameRateInput = 84,
    FrameRateOutput = 85,
    FrameRateReceived = 86,
    FrameRateSent = 87,
    FrameWidthInput = 88,
    FrameWidthReceived = 89,
    FrameWidthSent = 90,
    HasEnteredLowResolution = 91,
    HugeFramesSent = 92,
    Initiator = 93,
    InterframeDelayMaxMs = 94,
    IssuerId = 95,
    JitterBufferMs = 96,
    JitterReceived = 97,
    Label = 98,
    LocalAddress = 99,
    LocalCandidateId = 100,
    LocalCandidateType = 101,
    LocalCertificateId = 102,
    MaxDecodeMs = 103,
    MinPlayoutDelayMs = 104,
    NacksReceived = 105,
    NacksSent = 106,
    PlisReceived = 107,
    PlisSent = 108,
    PreemptiveExpandRate = 109,
    PreferredJitterBufferMs = 110,
    RemoteAddress = 111,
    RemoteCandidateId = 112,
    RemoteCandidateType = 113,
    RemoteCertificateId = 114,
    RenderDelayMs = 115,
    ResidualEchoLikelihood = 116,
    ResidualEchoLikelihoodRecentMax = 117,
    AnaBitrateActionCounter = 118,
    AnaChannelActionCounter = 119,
    AnaDtxActionCounter = 120,
    AnaFecActionCounter = 121,
    AnaFrameLengthIncreaseCounter = 122,
    AnaFrameLengthDecreaseCounter = 123,
    AnaUplinkPacketLossFraction = 124,
    RetransmitBitrate = 125,
    Rtt = 126,
    SecondaryDecodedRate = 127,
    SecondaryDiscardedRate = 128,
    SendPacketsDiscarded = 129,
    SpeechExpandRate = 130,
    SrtpCipher = 131,
    TargetDelayMs = 132,
    TargetEncBitrate = 133,
    TimingFrameInfo = 134,
    TrackId = 135,
    TransmitBitrate = 136,
    TransportType = 137,
    Writable = 138,
    AudioDeviceUnderrunCounter = 139,
    LocalCandidateRelayProtocol = 140,
}
impl LegacyStatsName {
    fn from_native(v: i32) -> Self {
        match v {
            0 => Self::ActiveConnection,
            1 => Self::AecDivergentFilterFraction,
            2 => Self::AudioInputLevel,
            3 => Self::AudioOutputLevel,
            4 => Self::BytesReceived,
            5 => Self::BytesSent,
            6 => Self::CodecImplementationName,
            7 => Self::ConcealedSamples,
            8 => Self::ConcealmentEvents,
            9 => Self::DataChannelId,
            10 => Self::FramesDecoded,
            11 => Self::FramesEncoded,
            12 => Self::JitterBufferDelay,
            13 => Self::MediaType,
            14 => Self::PacketsLost,
            15 => Self::PacketsReceived,
            16 => Self::PacketsSent,
            17 => Self::Protocol,
            18 => Self::QpSum,
            19 => Self::Receiving,
            20 => Self::SelectedCandidatePairId,
            21 => Self::Ssrc,
            22 => Self::State,
            23 => Self::TotalAudioEnergy,
            24 => Self::TotalSamplesDuration,
            25 => Self::TotalSamplesReceived,
            26 => Self::TransportId,
            27 => Self::SentPingRequestsTotal,
            28 => Self::SentPingRequestsBeforeFirstResponse,
            29 => Self::SentPingResponses,
            30 => Self::RecvPingRequests,
            31 => Self::RecvPingResponses,
            32 => Self::SentStunKeepaliveRequests,
            33 => Self::RecvStunKeepaliveResponses,
            34 => Self::StunKeepaliveRttTotal,
            35 => Self::StunKeepaliveRttSquaredTotal,
            36 => Self::AccelerateRate,
            37 => Self::ActualEncBitrate,
            38 => Self::AdaptationChanges,
            39 => Self::AvailableReceiveBandwidth,
            40 => Self::AvailableSendBandwidth,
            41 => Self::AvgEncodeMs,
            42 => Self::BandwidthLimitedResolution,
            43 => Self::BucketDelay,
            44 => Self::CaptureStartNtpTimeMs,
            45 => Self::CandidateIPAddress,
            46 => Self::CandidateNetworkType,
            47 => Self::CandidatePortNumber,
            48 => Self::CandidatePriority,
            49 => Self::CandidateTransportType,
            50 => Self::CandidateType,
            51 => Self::ChannelId,
            52 => Self::CodecName,
            53 => Self::Component,
            54 => Self::ContentName,
            55 => Self::ContentType,
            56 => Self::CpuLimitedResolution,
            57 => Self::CurrentDelayMs,
            58 => Self::DecodeMs,
            59 => Self::DecodingCNG,
            60 => Self::DecodingCTN,
            61 => Self::DecodingCTSG,
            62 => Self::DecodingMutedOutput,
            63 => Self::DecodingNormal,
            64 => Self::DecodingPLC,
            65 => Self::DecodingCodecPLC,
            66 => Self::DecodingPLCCNG,
            67 => Self::Der,
            68 => Self::DtlsCipher,
            69 => Self::EchoDelayMedian,
            70 => Self::EchoDelayStdDev,
            71 => Self::EchoReturnLoss,
            72 => Self::EchoReturnLossEnhancement,
            73 => Self::EncodeUsagePercent,
            74 => Self::ExpandRate,
            75 => Self::Fingerprint,
            76 => Self::FingerprintAlgorithm,
            77 => Self::FirsReceived,
            78 => Self::FirsSent,
            79 => Self::FirstFrameReceivedToDecodedMs,
            80 => Self::FrameHeightInput,
            81 => Self::FrameHeightReceived,
            82 => Self::FrameHeightSent,
            83 => Self::FrameRateDecoded,
            84 => Self::FrameRateInput,
            85 => Self::FrameRateOutput,
            86 => Self::FrameRateReceived,
            87 => Self::FrameRateSent,
            88 => Self::FrameWidthInput,
            89 => Self::FrameWidthReceived,
            90 => Self::FrameWidthSent,
            91 => Self::HasEnteredLowResolution,
            92 => Self::HugeFramesSent,
            93 => Self::Initiator,
            94 => Self::InterframeDelayMaxMs,
            95 => Self::IssuerId,
            96 => Self::JitterBufferMs,
            97 => Self::JitterReceived,
            98 => Self::Label,
            99 => Self::LocalAddress,
            100 => Self::LocalCandidateId,
            101 => Self::LocalCandidateType,
            102 => Self::LocalCertificateId,
            103 => Self::MaxDecodeMs,
            104 => Self::MinPlayoutDelayMs,
            105 => Self::NacksReceived,
            106 => Self::NacksSent,
            107 => Self::PlisReceived,
            108 => Self::PlisSent,
            109 => Self::PreemptiveExpandRate,
            110 => Self::PreferredJitterBufferMs,
            111 => Self::RemoteAddress,
            112 => Self::RemoteCandidateId,
            113 => Self::RemoteCandidateType,
            114 => Self::RemoteCertificateId,
            115 => Self::RenderDelayMs,
            116 => Self::ResidualEchoLikelihood,
            117 => Self::ResidualEchoLikelihoodRecentMax,
            118 => Self::AnaBitrateActionCounter,
            119 => Self::AnaChannelActionCounter,
            120 => Self::AnaDtxActionCounter,
            121 => Self::AnaFecActionCounter,
            122 => Self::AnaFrameLengthIncreaseCounter,
            123 => Self::AnaFrameLengthDecreaseCounter,
            124 => Self::AnaUplinkPacketLossFraction,
            125 => Self::RetransmitBitrate,
            126 => Self::Rtt,
            127 => Self::SecondaryDecodedRate,
            128 => Self::SecondaryDiscardedRate,
            129 => Self::SendPacketsDiscarded,
            130 => Self::SpeechExpandRate,
            131 => Self::SrtpCipher,
            132 => Self::TargetDelayMs,
            133 => Self::TargetEncBitrate,
            134 => Self::TimingFrameInfo,
            135 => Self::TrackId,
            136 => Self::TransmitBitrate,
            137 => Self::TransportType,
            138 => Self::Writable,
            139 => Self::AudioDeviceUnderrunCounter,
            140 => Self::LocalCandidateRelayProtocol,
            _ => unreachable!("pinned legacy value name"),
        }
    }
}

// IdBase implementations are immutable and make_ref_counted uses atomic refs.
// Unlike legacy Value refs, these SDK IDs have no sequence checker.
unsafe impl Send for ffi::NativeLegacyStatsId {}
unsafe impl Sync for ffi::NativeLegacyStatsId {}
pub struct LegacyStatsId(cxx::UniquePtr<ffi::NativeLegacyStatsId>);
impl LegacyStatsId {
    fn from_native(value: cxx::UniquePtr<ffi::NativeLegacyStatsId>) -> Self {
        assert!(!value.is_null());
        Self(value)
    }
    fn native(&self) -> &ffi::NativeLegacyStatsId {
        self.0.as_ref().unwrap()
    }
    pub fn kind(&self) -> LegacyStatsType {
        LegacyStatsType::from_native(ffi::legacy_stats_id_type(self.native()))
    }
    pub fn to_native_string(&self) -> Vec<u8> {
        ffi::legacy_stats_id_string(self.native())
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::legacy_stats_id_same(self.native(), other.native())
    }
}
impl Clone for LegacyStatsId {
    fn clone(&self) -> Self {
        Self(ffi::legacy_stats_id_copy(self.native()))
    }
}
impl PartialEq for LegacyStatsId {
    fn eq(&self, other: &Self) -> bool {
        ffi::legacy_stats_id_equal(self.native(), other.native())
    }
}
impl Eq for LegacyStatsId {}
impl fmt::Debug for LegacyStatsId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LegacyStatsId")
            .field("kind", &self.kind())
            .field("text", &self.to_native_string())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LegacyStatsValueData {
    Int(i32),
    Int64(i64),
    Float(f32),
    String(Vec<u8>),
    StaticString(Vec<u8>),
    Bool(bool),
    Id(LegacyStatsId),
}
#[derive(Clone, Debug, PartialEq)]
pub struct LegacyStatsValue {
    pub name: LegacyStatsName,
    pub display_name: Vec<u8>,
    pub value: LegacyStatsValueData,
    /// Actual SDK Value::ToString output, captured before callback return.
    pub native_string: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct LegacyStatsReport {
    pub id: LegacyStatsId,
    pub timestamp_ms: f64,
    pub native_type: Vec<u8>,
    /// Native ordered name/value mapping; all seven value kinds are preserved.
    pub values: Vec<LegacyStatsValue>,
}
impl LegacyStatsReport {
    pub fn find(&self, name: LegacyStatsName) -> Option<&LegacyStatsValue> {
        self.values.iter().find(|v| v.name == name)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct PeerLegacyStatsSnapshot {
    pub operation_id: OperationId,
    pub reports: Vec<LegacyStatsReport>,
}
impl PeerLegacyStatsSnapshot {
    pub(crate) fn from_native(id: OperationId, reports: Vec<ffi::FfiLegacyStatsReport>) -> Self {
        Self {
            operation_id: id,
            reports: reports
                .into_iter()
                .map(|r| LegacyStatsReport {
                    id: LegacyStatsId::from_native(r.id),
                    timestamp_ms: r.timestamp_ms,
                    native_type: r.native_type,
                    values: r
                        .values
                        .into_iter()
                        .map(|v| LegacyStatsValue {
                            name: LegacyStatsName::from_native(v.name),
                            display_name: v.display_name,
                            native_string: v.native_string,
                            value: match v.kind {
                                0 => LegacyStatsValueData::Int(
                                    i32::try_from(v.integer).expect("native int"),
                                ),
                                1 => LegacyStatsValueData::Int64(v.integer),
                                2 => LegacyStatsValueData::Float(v.decimal),
                                3 => LegacyStatsValueData::String(v.text),
                                4 => LegacyStatsValueData::StaticString(v.text),
                                5 => LegacyStatsValueData::Bool(v.flag),
                                6 => LegacyStatsValueData::Id(LegacyStatsId::from_native(v.id)),
                                _ => unreachable!("pinned legacy value type"),
                            },
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}
