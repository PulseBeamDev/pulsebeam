use std::num::NonZeroUsize;

use crate::{EgressSlot, IngressStream, MediaDirection, MediaKind};

const MAX_NEGOTIATED_STREAM_SLOTS: u32 = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcConfiguration {
    max_ingress_streams: u32,
    max_egress_slots: u32,
    max_events: u32,
    max_transmissions: u32,
}

impl Default for RtcConfiguration {
    fn default() -> Self {
        Self {
            max_ingress_streams: 256,
            max_egress_slots: 256,
            max_events: 256,
            max_transmissions: 256,
        }
    }
}

impl RtcConfiguration {
    pub fn new(
        max_ingress_streams: u32,
        max_egress_slots: u32,
        max_events: u32,
        max_transmissions: u32,
    ) -> Option<Self> {
        if max_ingress_streams == 0
            || max_egress_slots == 0
            || max_ingress_streams > MAX_NEGOTIATED_STREAM_SLOTS
            || max_egress_slots > MAX_NEGOTIATED_STREAM_SLOTS
            || max_events == 0
            || max_transmissions == 0
        {
            None
        } else {
            Some(Self {
                max_ingress_streams,
                max_egress_slots,
                max_events,
                max_transmissions,
            })
        }
    }

    pub const fn max_ingress_streams(&self) -> u32 {
        self.max_ingress_streams
    }
    pub const fn max_egress_slots(&self) -> u32 {
        self.max_egress_slots
    }
    pub const fn max_events(&self) -> u32 {
        self.max_events
    }
    pub const fn max_transmissions(&self) -> u32 {
        self.max_transmissions
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiationParameters {
    local_ice: IceCredentials,
    local_fingerprint: DtlsFingerprint,
    local_candidates: Box<[IceCandidate]>,
}

impl NegotiationParameters {
    pub fn new(
        local_ice: IceCredentials,
        local_fingerprint: DtlsFingerprint,
        local_candidates: Box<[IceCandidate]>,
    ) -> Self {
        Self {
            local_ice,
            local_fingerprint,
            local_candidates,
        }
    }
    pub fn local_ice(&self) -> &IceCredentials {
        &self.local_ice
    }
    pub fn local_fingerprint(&self) -> &DtlsFingerprint {
        &self.local_fingerprint
    }
    pub fn local_candidates(&self) -> &[IceCandidate] {
        &self.local_candidates
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IceCandidate(String);

impl IceCandidate {
    pub fn new(value: String) -> Option<Self> {
        if value.trim().is_empty() || !value.is_ascii() || value.contains(['\r', '\n']) {
            None
        } else {
            Some(Self(value))
        }
    }
    pub fn as_sdp(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IceCredentials {
    ufrag: String,
    password: String,
}

impl IceCredentials {
    pub fn new(ufrag: String, password: String) -> Option<Self> {
        if ufrag.trim().is_empty()
            || password.trim().is_empty()
            || !(4..=256).contains(&ufrag.len())
            || !(22..=256).contains(&password.len())
            || !ufrag.bytes().all(ice_char)
            || !password.bytes().all(ice_char)
        {
            None
        } else {
            Some(Self { ufrag, password })
        }
    }

    pub fn ufrag(&self) -> &str {
        &self.ufrag
    }
    pub fn password(&self) -> &str {
        &self.password
    }
}

fn ice_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtlsFingerprint {
    algorithm: String,
    value: Box<[u8]>,
}

impl DtlsFingerprint {
    pub fn new(algorithm: String, value: Box<[u8]>) -> Option<Self> {
        if algorithm.chars().any(|c| c == '\r' || c == '\n') {
            return None;
        }
        let algorithm = algorithm.to_ascii_lowercase();
        let expected = match algorithm.as_str() {
            "sha-256" => 32,
            "sha-384" => 48,
            "sha-512" => 64,
            _ => return None,
        };
        (value.len() == expected).then_some(Self { algorithm, value })
    }

    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtlsRole {
    Active,
    Passive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Codec {
    payload_type: u8,
    name: String,
    clock_rate: u32,
    channels: Option<u8>,
}

impl Codec {
    pub fn new(
        payload_type: u8,
        name: String,
        clock_rate: u32,
        channels: Option<u8>,
    ) -> Option<Self> {
        if name.trim().is_empty() || clock_rate == 0 || channels == Some(0) {
            None
        } else {
            Some(Self {
                payload_type,
                name,
                clock_rate,
                channels,
            })
        }
    }

    pub const fn payload_type(&self) -> u8 {
        self.payload_type
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub const fn clock_rate(&self) -> u32 {
        self.clock_rate
    }
    pub const fn channels(&self) -> Option<u8> {
        self.channels
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderExtension {
    id: u8,
    uri: String,
    direction: MediaDirection,
}

impl HeaderExtension {
    pub fn new(id: u8, uri: String) -> Option<Self> {
        Self::with_direction(id, uri, MediaDirection::Bidirectional)
    }

    pub(crate) fn with_direction(id: u8, uri: String, direction: MediaDirection) -> Option<Self> {
        if id == 0 || uri.trim().is_empty() {
            None
        } else {
            Some(Self { id, uri, direction })
        }
    }

    pub const fn id(&self) -> u8 {
        self.id
    }
    pub fn uri(&self) -> &str {
        &self.uri
    }
    pub const fn direction(&self) -> MediaDirection {
        self.direction
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MaxMessageSize {
    #[default]
    Default,
    Finite(NonZeroUsize),
    Unlimited,
}

impl MaxMessageSize {
    pub fn finite(value: usize) -> Option<Self> {
        NonZeroUsize::new(value).map(Self::Finite)
    }
    pub const fn is_unlimited(self) -> bool {
        matches!(self, Self::Unlimited)
    }
    pub const fn finite_value(self) -> Option<usize> {
        match self {
            Self::Finite(value) => Some(value.get()),
            Self::Default | Self::Unlimited => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataChannelParameters {
    sctp_port: u16,
    max_message_size: MaxMessageSize,
}

impl DataChannelParameters {
    pub fn new(sctp_port: u16, max_message_size: MaxMessageSize) -> Option<Self> {
        if sctp_port == 0 {
            None
        } else {
            Some(Self {
                sctp_port,
                max_message_size,
            })
        }
    }

    pub const fn sctp_port(&self) -> u16 {
        self.sctp_port
    }
    pub const fn max_message_size(&self) -> MaxMessageSize {
        self.max_message_size
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiatedCodec {
    name: String,
    payload_type: u8,
    clock_rate: u32,
    channels: Option<u8>,
    retransmission_payload_type: Option<u8>,
    transport_cc: bool,
    nack: bool,
    pli: bool,
    fir: bool,
    h264: Option<H264Parameters>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct H264Parameters {
    packetization_mode: Option<u8>,
    profile_level_id: Option<String>,
    level_asymmetry_allowed: Option<bool>,
}

impl H264Parameters {
    pub(crate) fn new(
        packetization_mode: Option<u8>,
        profile_level_id: Option<String>,
        level_asymmetry_allowed: Option<bool>,
    ) -> Self {
        Self {
            packetization_mode,
            profile_level_id,
            level_asymmetry_allowed,
        }
    }
    pub const fn packetization_mode(&self) -> Option<u8> {
        self.packetization_mode
    }
    pub fn profile_level_id(&self) -> Option<&str> {
        self.profile_level_id.as_deref()
    }
    pub const fn level_asymmetry_allowed(&self) -> Option<bool> {
        self.level_asymmetry_allowed
    }
}

impl NegotiatedCodec {
    #[allow(
        clippy::too_many_arguments,
        reason = "codec facts are an immutable negotiated tuple"
    )]
    pub(crate) fn new(
        name: String,
        payload_type: u8,
        clock_rate: u32,
        channels: Option<u8>,
        retransmission_payload_type: Option<u8>,
        transport_cc: bool,
        nack: bool,
        pli: bool,
        fir: bool,
        h264: Option<H264Parameters>,
    ) -> Self {
        Self {
            name,
            payload_type,
            clock_rate,
            channels,
            retransmission_payload_type,
            transport_cc,
            nack,
            pli,
            fir,
            h264,
        }
    }
    pub(crate) fn with_retransmission_payload_type(mut self, payload_type: Option<u8>) -> Self {
        self.retransmission_payload_type = payload_type;
        self
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub const fn payload_type(&self) -> u8 {
        self.payload_type
    }
    pub const fn clock_rate(&self) -> u32 {
        self.clock_rate
    }
    pub const fn channels(&self) -> Option<u8> {
        self.channels
    }
    pub const fn retransmission_payload_type(&self) -> Option<u8> {
        self.retransmission_payload_type
    }
    pub const fn transport_cc(&self) -> bool {
        self.transport_cc
    }
    pub const fn nack(&self) -> bool {
        self.nack
    }
    pub const fn pli(&self) -> bool {
        self.pli
    }
    pub const fn fir(&self) -> bool {
        self.fir
    }
    pub const fn h264(&self) -> Option<&H264Parameters> {
        self.h264.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiatedMedia {
    ingress: Option<IngressStream>,
    egress: Option<EgressSlot>,
    mid: String,
    rids: Box<[String]>,
    kind: MediaKind,
    direction: MediaDirection,
    codecs: Box<[NegotiatedCodec]>,
}

impl NegotiatedMedia {
    pub(crate) fn new(
        ingress: Option<IngressStream>,
        egress: Option<EgressSlot>,
        mid: String,
        rids: Box<[String]>,
        kind: MediaKind,
        direction: MediaDirection,
        codecs: Box<[NegotiatedCodec]>,
    ) -> Self {
        Self {
            ingress,
            egress,
            mid,
            rids,
            kind,
            direction,
            codecs,
        }
    }
    pub const fn ingress(&self) -> Option<IngressStream> {
        self.ingress
    }
    pub const fn egress(&self) -> Option<EgressSlot> {
        self.egress
    }
    pub fn mid(&self) -> &str {
        &self.mid
    }
    pub fn rids(&self) -> &[String] {
        &self.rids
    }
    pub const fn kind(&self) -> MediaKind {
        self.kind
    }
    pub const fn direction(&self) -> MediaDirection {
        self.direction
    }
    pub fn codecs(&self) -> &[NegotiatedCodec] {
        &self.codecs
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsrcGroup {
    semantics: String,
    members: Box<[u32]>,
}

impl SsrcGroup {
    pub(crate) fn new(semantics: String, members: Box<[u32]>) -> Self {
        Self { semantics, members }
    }
    pub fn semantics(&self) -> &str {
        &self.semantics
    }
    pub fn members(&self) -> &[u32] {
        &self.members
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiatedMediaSection {
    mid: String,
    kind: MediaKind,
    direction: MediaDirection,
    codecs: Box<[Codec]>,
    header_extensions: Box<[HeaderExtension]>,
    receive_rids: Box<[String]>,
    ssrcs: Box<[u32]>,
    has_ssrc_zero_probe: bool,
    ssrc_groups: Box<[SsrcGroup]>,
    data_channel: Option<DataChannelParameters>,
}

impl NegotiatedMediaSection {
    #[allow(
        clippy::too_many_arguments,
        reason = "section facts are one immutable negotiated tuple"
    )]
    pub(crate) fn new(
        mid: String,
        kind: MediaKind,
        direction: MediaDirection,
        codecs: Box<[Codec]>,
        header_extensions: Box<[HeaderExtension]>,
        receive_rids: Box<[String]>,
        ssrcs: Box<[u32]>,
        has_ssrc_zero_probe: bool,
        ssrc_groups: Box<[SsrcGroup]>,
        data_channel: Option<DataChannelParameters>,
    ) -> Self {
        Self {
            mid,
            kind,
            direction,
            codecs,
            header_extensions,
            receive_rids,
            ssrcs,
            has_ssrc_zero_probe,
            ssrc_groups,
            data_channel,
        }
    }
    pub fn mid(&self) -> &str {
        &self.mid
    }
    pub const fn kind(&self) -> MediaKind {
        self.kind
    }
    pub const fn direction(&self) -> MediaDirection {
        self.direction
    }
    pub fn codecs(&self) -> &[Codec] {
        &self.codecs
    }
    pub fn header_extensions(&self) -> &[HeaderExtension] {
        &self.header_extensions
    }
    pub fn receive_rids(&self) -> &[String] {
        &self.receive_rids
    }
    pub fn ssrcs(&self) -> &[u32] {
        &self.ssrcs
    }
    pub const fn has_ssrc_zero_probe(&self) -> bool {
        self.has_ssrc_zero_probe
    }
    pub fn ssrc_groups(&self) -> &[SsrcGroup] {
        &self.ssrc_groups
    }
    pub fn data_channel(&self) -> Option<&DataChannelParameters> {
        self.data_channel.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiatedSession {
    local_ice: IceCredentials,
    remote_ice: IceCredentials,
    local_candidates: Box<[IceCandidate]>,
    remote_candidates: Box<[IceCandidate]>,
    local_fingerprint: DtlsFingerprint,
    local_dtls_role: DtlsRole,
    remote_fingerprint: DtlsFingerprint,
    media_sections: Box<[NegotiatedMediaSection]>,
}

impl NegotiatedSession {
    #[allow(
        clippy::too_many_arguments,
        reason = "session construction receives the immutable negotiated transport tuple"
    )]
    pub(crate) fn new(
        local_ice: IceCredentials,
        remote_ice: IceCredentials,
        local_candidates: Box<[IceCandidate]>,
        remote_candidates: Box<[IceCandidate]>,
        local_fingerprint: DtlsFingerprint,
        local_dtls_role: DtlsRole,
        remote_fingerprint: DtlsFingerprint,
        media_sections: Box<[NegotiatedMediaSection]>,
    ) -> Self {
        Self {
            local_ice,
            remote_ice,
            local_candidates,
            remote_candidates,
            local_fingerprint,
            local_dtls_role,
            remote_fingerprint,
            media_sections,
        }
    }
    pub fn media_sections(&self) -> &[NegotiatedMediaSection] {
        &self.media_sections
    }
    pub fn local_ice(&self) -> &IceCredentials {
        &self.local_ice
    }
    pub fn remote_ice(&self) -> &IceCredentials {
        &self.remote_ice
    }
    pub fn local_candidates(&self) -> &[IceCandidate] {
        &self.local_candidates
    }
    pub fn remote_candidates(&self) -> &[IceCandidate] {
        &self.remote_candidates
    }
    pub fn local_fingerprint(&self) -> &DtlsFingerprint {
        &self.local_fingerprint
    }
    pub fn remote_fingerprint(&self) -> &DtlsFingerprint {
        &self.remote_fingerprint
    }
    pub const fn local_dtls_role(&self) -> DtlsRole {
        self.local_dtls_role
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdpAnswer(String);
impl SdpAnswer {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcNegotiation {
    answer: SdpAnswer,
    media: Box<[NegotiatedMedia]>,
    session: NegotiatedSession,
}
impl RtcNegotiation {
    pub(crate) fn new(
        answer: SdpAnswer,
        media: Box<[NegotiatedMedia]>,
        session: NegotiatedSession,
    ) -> Self {
        Self {
            answer,
            media,
            session,
        }
    }
    pub fn answer(&self) -> &str {
        self.answer.as_str()
    }
    pub fn media(&self) -> &[NegotiatedMedia] {
        &self.media
    }
    pub const fn session(&self) -> &NegotiatedSession {
        &self.session
    }
}
