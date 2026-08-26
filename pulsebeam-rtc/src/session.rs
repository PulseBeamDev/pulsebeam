use crate::MediaSectionId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IceCredentials {
    ufrag: String,
    password: String,
}

impl IceCredentials {
    pub fn new(ufrag: String, password: String) -> Option<Self> {
        if ufrag.is_empty() || password.is_empty() {
            return None;
        }

        Some(Self { ufrag, password })
    }

    pub fn ufrag(&self) -> &str {
        &self.ufrag
    }

    pub fn password(&self) -> &str {
        &self.password
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtlsFingerprint {
    algorithm: String,
    value: Box<[u8]>,
}

impl DtlsFingerprint {
    pub fn new(algorithm: String, value: Box<[u8]>) -> Option<Self> {
        if algorithm.is_empty() || value.is_empty() {
            return None;
        }

        Some(Self { algorithm, value })
    }

    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IceCandidate(String);

impl IceCandidate {
    pub fn new(value: String) -> Option<Self> {
        if value.is_empty() {
            return None;
        }

        Some(Self(value))
    }

    pub fn as_sdp(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
    Application,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaDirection {
    SendOnly,
    ReceiveOnly,
    Inactive,
    Bidirectional,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Codec {
    payload_type: u8,
    name: String,
    clock_rate: u32,
    channels: Option<u8>,
    retransmission_payload_type: Option<u8>,
    transport_cc: bool,
    nack: bool,
    pli: bool,
    fir: bool,
}

impl Codec {
    pub fn payload_type(&self) -> u8 {
        self.payload_type
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn clock_rate(&self) -> u32 {
        self.clock_rate
    }

    pub fn channels(&self) -> Option<u8> {
        self.channels
    }

    pub fn retransmission_payload_type(&self) -> Option<u8> {
        self.retransmission_payload_type
    }

    pub fn transport_cc(&self) -> bool {
        self.transport_cc
    }

    pub fn nack(&self) -> bool {
        self.nack
    }

    pub fn pli(&self) -> bool {
        self.pli
    }

    pub fn fir(&self) -> bool {
        self.fir
    }

    pub(crate) fn new(
        payload_type: u8,
        name: String,
        clock_rate: u32,
        channels: Option<u8>,
        retransmission_payload_type: Option<u8>,
        transport_cc: bool,
        nack: bool,
        pli: bool,
        fir: bool,
    ) -> Self {
        Self {
            payload_type,
            name,
            clock_rate,
            channels,
            retransmission_payload_type,
            transport_cc,
            nack,
            pli,
            fir,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderExtension {
    id: u8,
    uri: String,
}

impl HeaderExtension {
    pub fn id(&self) -> u8 {
        self.id
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub(crate) fn new(id: u8, uri: String) -> Self {
        Self { id, uri }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataChannelParameters {
    sctp_port: u16,
    max_message_size: Option<usize>,
}

impl DataChannelParameters {
    pub fn sctp_port(&self) -> u16 {
        self.sctp_port
    }

    pub fn max_message_size(&self) -> Option<usize> {
        self.max_message_size
    }

    pub(crate) fn new(sctp_port: u16, max_message_size: Option<usize>) -> Self {
        Self {
            sctp_port,
            max_message_size,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiatedMediaSection {
    id: MediaSectionId,
    mid: String,
    kind: MediaKind,
    direction: MediaDirection,
    codecs: Box<[Codec]>,
    header_extensions: Box<[HeaderExtension]>,
    data_channel: Option<DataChannelParameters>,
}

impl NegotiatedMediaSection {
    pub fn id(&self) -> MediaSectionId {
        self.id
    }

    pub fn mid(&self) -> &str {
        &self.mid
    }

    pub fn kind(&self) -> MediaKind {
        self.kind
    }

    pub fn direction(&self) -> MediaDirection {
        self.direction
    }

    pub fn codecs(&self) -> &[Codec] {
        &self.codecs
    }

    pub fn header_extensions(&self) -> &[HeaderExtension] {
        &self.header_extensions
    }

    pub fn data_channel(&self) -> Option<&DataChannelParameters> {
        self.data_channel.as_ref()
    }

    pub(crate) fn new(
        id: MediaSectionId,
        mid: String,
        kind: MediaKind,
        direction: MediaDirection,
        codecs: Box<[Codec]>,
        header_extensions: Box<[HeaderExtension]>,
        data_channel: Option<DataChannelParameters>,
    ) -> Self {
        Self {
            id,
            mid,
            kind,
            direction,
            codecs,
            header_extensions,
            data_channel,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiatedSession {
    local_ice: IceCredentials,
    local_fingerprint: DtlsFingerprint,
    local_candidates: Box<[IceCandidate]>,
    remote_ice: IceCredentials,
    remote_fingerprint: DtlsFingerprint,
    remote_candidates: Box<[IceCandidate]>,
    media_sections: Box<[NegotiatedMediaSection]>,
}

impl NegotiatedSession {
    pub fn local_ice(&self) -> &IceCredentials {
        &self.local_ice
    }

    pub fn local_fingerprint(&self) -> &DtlsFingerprint {
        &self.local_fingerprint
    }

    pub fn local_candidates(&self) -> &[IceCandidate] {
        &self.local_candidates
    }

    pub fn remote_ice(&self) -> &IceCredentials {
        &self.remote_ice
    }

    pub fn remote_fingerprint(&self) -> &DtlsFingerprint {
        &self.remote_fingerprint
    }

    pub fn remote_candidates(&self) -> &[IceCandidate] {
        &self.remote_candidates
    }

    pub fn media_sections(&self) -> &[NegotiatedMediaSection] {
        &self.media_sections
    }

    pub(crate) fn new(
        local_ice: IceCredentials,
        local_fingerprint: DtlsFingerprint,
        local_candidates: Box<[IceCandidate]>,
        remote_ice: IceCredentials,
        remote_fingerprint: DtlsFingerprint,
        remote_candidates: Box<[IceCandidate]>,
        media_sections: Box<[NegotiatedMediaSection]>,
    ) -> Self {
        Self {
            local_ice,
            local_fingerprint,
            local_candidates,
            remote_ice,
            remote_fingerprint,
            remote_candidates,
            media_sections,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdpAnswer(String);

impl SdpAnswer {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }
}
