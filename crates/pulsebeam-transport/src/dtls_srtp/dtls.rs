use std::collections::VecDeque;
use std::ops::RangeInclusive;
use std::time::Instant;

use pulsebeam_dtls::{Association, LocalIdentity, Output, Role, SrtpVersion};
#[cfg(test)]
use str0m::crypto::dtls::ProtocolVersion;
use str0m::crypto::dtls::{DtlsCert, DtlsVersion, KeyingMaterial, SrtpProfile};
use str0m::crypto::{CryptoProvider, Fingerprint};

const DTLS_MTU: RangeInclusive<usize> = 1200..=1500;
const DTLS_POLL_BUFFER: usize = 16 * 1024;
const MAX_OUTPUT: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub enum DtlsError {
    Crypto,
    BufferTooSmall,
    FingerprintMismatch,
    InvalidState,
    OutputFull,
}

#[derive(Debug)]
pub enum DtlsEvent {
    Connected,
    KeyingMaterial(KeyingMaterial, SrtpProfile),
    ApplicationData(Vec<u8>),
    CloseNotify,
}

// Bounded transport output ownership only; all DTLS/authentication policy lives
// in pulsebeam-dtls, including the SRTP exporter authentication gate.
pub(crate) struct DtlsLayer {
    instance: Association,
    connected: bool,
    closing: bool,
    closed: bool,
    packets: VecDeque<Vec<u8>>,
    events: VecDeque<DtlsEvent>,
    poll_buffer: Vec<u8>,
    next_deadline: Option<Instant>,
}

impl DtlsLayer {
    pub(crate) fn new(
        certificate: DtlsCert,
        remote_fingerprint: Fingerprint,
        active: bool,
        now: Instant,
        provider: &CryptoProvider,
    ) -> Result<Self, DtlsError> {
        Self::new_with_version(
            certificate,
            remote_fingerprint,
            active,
            now,
            provider,
            DtlsVersion::Auto,
        )
    }

    fn new_with_version(
        certificate: DtlsCert,
        remote_fingerprint: Fingerprint,
        active: bool,
        now: Instant,
        _provider: &CryptoProvider,
        version: DtlsVersion,
    ) -> Result<Self, DtlsError> {
        if remote_fingerprint.hash_func != "sha-256" {
            return Err(DtlsError::InvalidState);
        }
        let pin = remote_fingerprint
            .bytes
            .as_slice()
            .try_into()
            .map_err(|_| DtlsError::InvalidState)?;
        let local = LocalIdentity::from_der(certificate.certificate, certificate.private_key)
            .map_err(map_error)?;
        let version = match version {
            DtlsVersion::Auto => SrtpVersion::Auto,
            DtlsVersion::Dtls12 => SrtpVersion::Dtls12,
            DtlsVersion::Dtls13 => SrtpVersion::Dtls13,
            _ => return Err(DtlsError::InvalidState),
        };
        let mut instance = Association::dtls_srtp(
            if active { Role::Client } else { Role::Server },
            &local,
            pulsebeam_dtls::Fingerprint::from_bytes(pin),
            *DTLS_MTU.start(),
            now,
            version,
        )
        .map_err(map_error)?;
        instance.handle_timeout(now).map_err(map_error)?;
        let mut layer = Self {
            instance,
            connected: false,
            closing: false,
            closed: false,
            packets: VecDeque::new(),
            events: VecDeque::new(),
            poll_buffer: vec![0; DTLS_POLL_BUFFER],
            next_deadline: None,
        };
        layer.drive()?;
        Ok(layer)
    }

    pub(crate) fn connected(&self) -> bool {
        self.connected
    }
    #[cfg(test)]
    pub(crate) fn protocol_version(&self) -> Option<ProtocolVersion> {
        self.instance
            .protocol_version()
            .and_then(|version| match version {
                pulsebeam_dtls::ProtocolVersion::DTLS1_2 => Some(ProtocolVersion::DTLS1_2),
                pulsebeam_dtls::ProtocolVersion::DTLS1_3 => Some(ProtocolVersion::DTLS1_3),
                _ => None,
            })
    }
    #[cfg(test)]
    pub(crate) fn negotiated_profile(&self) -> Option<SrtpProfile> {
        self.instance
            .negotiated_srtp_profile()
            .and_then(|profile| convert_profile(profile).ok())
    }
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.next_deadline
    }
    fn tick(&mut self, now: Instant) -> Result<(), DtlsError> {
        self.instance.handle_timeout(now).map_err(map_error)?;
        self.drive()
    }
    pub(crate) fn handle_packet(&mut self, packet: &[u8], now: Instant) -> Result<(), DtlsError> {
        let Some(&kind) = packet.first() else {
            return Err(DtlsError::InvalidState);
        };
        if self.closed || self.closing || packet.len() < 13 || !(20..=64).contains(&kind) {
            return Err(DtlsError::InvalidState);
        }
        self.tick(now)?;
        self.instance.handle_packet(packet).map_err(map_error)?;
        self.drive()
    }
    pub(crate) fn handle_timeout(&mut self, now: Instant) -> Result<(), DtlsError> {
        if self.closed || self.closing {
            return Ok(());
        }
        if self.next_deadline.is_none_or(|at| at > now) {
            return Err(DtlsError::InvalidState);
        }
        self.tick(now)
    }
    pub(crate) fn close(&mut self, now: Instant) -> Result<(), DtlsError> {
        if self.closed {
            return Ok(());
        }
        self.tick(now)?;
        self.instance.shutdown().map_err(map_error)?;
        self.closing = true;
        self.next_deadline = None;
        self.drive()
    }
    pub(crate) fn send_application_data(
        &mut self,
        data: &[u8],
        now: Instant,
    ) -> Result<(), DtlsError> {
        if !self.connected || self.closing || self.closed {
            return Err(DtlsError::InvalidState);
        }
        self.tick(now)?;
        self.instance
            .send_application_data(data)
            .map_err(map_error)?;
        self.drive()
    }
    pub(crate) fn poll_packet(&mut self) -> Option<Vec<u8>> {
        self.packets.pop_front()
    }
    pub(crate) fn poll_event(&mut self) -> Option<DtlsEvent> {
        self.events.pop_front()
    }
    pub(crate) fn clear_pending(&mut self) {
        self.packets.clear();
        self.events.clear();
    }

    fn drive(&mut self) -> Result<(), DtlsError> {
        let mut buffer = std::mem::take(&mut self.poll_buffer);
        let result = self.drive_with_buffer(&mut buffer);
        self.poll_buffer = buffer;
        result
    }
    fn drive_with_buffer(&mut self, buffer: &mut [u8]) -> Result<(), DtlsError> {
        for _ in 0..MAX_OUTPUT {
            match self.instance.poll_output(buffer).map_err(map_error)? {
                Output::Datagram(packet) => {
                    if packet.is_empty()
                        || packet.len() > *DTLS_MTU.end()
                        || self.packets.len() >= MAX_OUTPUT
                    {
                        return Err(DtlsError::OutputFull);
                    }
                    self.packets.push_back(packet.to_vec());
                }
                Output::BufferTooSmall { .. } => return Err(DtlsError::BufferTooSmall),
                Output::Timeout(at) => {
                    self.next_deadline = (!self.connected && !self.closing).then_some(at);
                    return Ok(());
                }
                Output::Connected => {
                    self.connected = true;
                    self.enqueue_event(DtlsEvent::Connected)?;
                    self.next_deadline = None;
                }
                Output::KeyingMaterial(material, profile) => {
                    let material = KeyingMaterial::new(&material);
                    self.enqueue_event(DtlsEvent::KeyingMaterial(
                        material,
                        convert_profile(profile)?,
                    ))?;
                }
                Output::ApplicationData(bytes) => {
                    let bytes = bytes.to_vec();
                    self.enqueue_event(DtlsEvent::ApplicationData(bytes))?;
                }
                Output::Progress => {}
                Output::Closed => {
                    self.connected = false;
                    self.closing = true;
                    self.closed = true;
                    self.next_deadline = None;
                    self.enqueue_event(DtlsEvent::CloseNotify)?;
                    return Ok(());
                }
            }
        }
        Err(DtlsError::OutputFull)
    }
    fn enqueue_event(&mut self, event: DtlsEvent) -> Result<(), DtlsError> {
        if self.events.len() >= MAX_OUTPUT {
            return Err(DtlsError::OutputFull);
        }
        self.events.push_back(event);
        Ok(())
    }
}

fn map_error(error: pulsebeam_dtls::Error) -> DtlsError {
    match error {
        pulsebeam_dtls::Error::PeerMismatch => DtlsError::FingerprintMismatch,
        pulsebeam_dtls::Error::TimeWentBackwards
        | pulsebeam_dtls::Error::NotConnected
        | pulsebeam_dtls::Error::Closed => DtlsError::InvalidState,
        _ => DtlsError::Crypto,
    }
}
fn convert_profile(profile: pulsebeam_dtls::SrtpProfile) -> Result<SrtpProfile, DtlsError> {
    Ok(match profile {
        pulsebeam_dtls::SrtpProfile::AES128_CM_SHA1_80 => SrtpProfile::AES128_CM_SHA1_80,
        pulsebeam_dtls::SrtpProfile::AEAD_AES_128_GCM => SrtpProfile::AEAD_AES_128_GCM,
        pulsebeam_dtls::SrtpProfile::AEAD_AES_256_GCM => SrtpProfile::AEAD_AES_256_GCM,
        _ => return Err(DtlsError::Crypto),
    })
}

#[cfg(test)]
mod tests {
    use super::super::srtp::SrtpLayer;
    use super::*;

    struct ConnectedPair {
        client: DtlsLayer,
        server: DtlsLayer,
        client_material: Option<KeyingMaterial>,
        server_material: Option<KeyingMaterial>,
        profile: SrtpProfile,
        server_packets: Vec<Vec<u8>>,
        now: Instant,
    }

    fn fingerprint(certificate: &DtlsCert, provider: &CryptoProvider) -> Fingerprint {
        Fingerprint {
            hash_func: "sha-256".to_owned(),
            bytes: provider
                .sha256_provider
                .sha256(&certificate.certificate)
                .to_vec(),
        }
    }

    fn connect(server_version: DtlsVersion) -> ConnectedPair {
        let provider = str0m::crypto::from_feature_flags();
        let client_certificate = provider
            .dtls_provider
            .generate_certificate()
            .expect("provider generates client certificate");
        let server_certificate = provider
            .dtls_provider
            .generate_certificate()
            .expect("provider generates server certificate");
        let mut now = Instant::now();
        let client_fingerprint = fingerprint(&client_certificate, &provider);
        let server_fingerprint = fingerprint(&server_certificate, &provider);
        let mut client =
            DtlsLayer::new(client_certificate, server_fingerprint, true, now, &provider)
                .expect("automatic client");
        let mut server = DtlsLayer::new_with_version(
            server_certificate,
            client_fingerprint,
            false,
            now,
            &provider,
            server_version,
        )
        .expect("controlled server");
        let mut client_material = None;
        let mut server_material = None;
        let mut profile = None;
        let mut server_packets = Vec::new();

        for _ in 0..400 {
            let mut progress = false;
            while let Some(packet) = client.poll_packet() {
                progress = true;
                server
                    .handle_packet(&packet, now)
                    .expect("server handles client DTLS");
            }
            while let Some(packet) = server.poll_packet() {
                progress = true;
                server_packets.push(packet.clone());
                client
                    .handle_packet(&packet, now)
                    .expect("client handles server DTLS");
            }
            while let Some(event) = client.poll_event() {
                if let DtlsEvent::KeyingMaterial(material, selected) = event {
                    client_material = Some(material);
                    profile = Some(selected);
                }
            }
            while let Some(event) = server.poll_event() {
                if let DtlsEvent::KeyingMaterial(material, selected) = event {
                    server_material = Some(material);
                    assert!(profile.is_none_or(|profile| profile == selected));
                    profile = Some(selected);
                }
            }
            if client.connected()
                && server.connected()
                && client_material.is_some()
                && server_material.is_some()
            {
                return ConnectedPair {
                    client,
                    server,
                    client_material,
                    server_material,
                    profile: profile.expect("negotiated SRTP profile"),
                    server_packets,
                    now,
                };
            }
            if !progress {
                now = now
                    .checked_add(std::time::Duration::from_millis(50))
                    .expect("test clock");
                if client
                    .next_deadline()
                    .is_some_and(|deadline| deadline <= now)
                {
                    client.handle_timeout(now).expect("client timeout");
                }
                if server
                    .next_deadline()
                    .is_some_and(|deadline| deadline <= now)
                {
                    server.handle_timeout(now).expect("server timeout");
                }
            }
        }
        panic!("DTLS peers did not connect")
    }

    fn server_hello_cipher_suite(packets: &[Vec<u8>]) -> Option<u16> {
        packets.iter().find_map(|packet| {
            let mut record = packet.as_slice();
            while record.len() >= 13 {
                let record_len = usize::from(u16::from_be_bytes([record[11], record[12]]));
                let payload = record.get(13..13usize.checked_add(record_len)?)?;
                if record[0] == 22 && payload.first() == Some(&2) && payload.len() >= 47 {
                    let session_id_len = usize::from(*payload.get(46)?);
                    let cipher_start = 47usize.checked_add(session_id_len)?;
                    return Some(u16::from_be_bytes([
                        *payload.get(cipher_start)?,
                        *payload.get(cipher_start.checked_add(1)?)?,
                    ]));
                }
                record = record.get(13usize.checked_add(record_len)?..)?;
            }
            None
        })
    }

    fn assert_protected_traffic(pair: &mut ConnectedPair) {
        let provider = str0m::crypto::from_feature_flags();
        let client_material = pair.client_material.take().expect("client keying material");
        let server_material = pair.server_material.take().expect("server keying material");
        let mut client_srtp = SrtpLayer::new(
            client_material,
            pair.profile,
            true,
            &provider,
            super::super::srtp::ReplayLimits::default(),
            0,
        )
        .expect("client SRTP");
        let mut server_srtp = SrtpLayer::new(
            server_material,
            pair.profile,
            false,
            &provider,
            super::super::srtp::ReplayLimits::default(),
            0,
        )
        .expect("server SRTP");
        let rtp = [0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7, 1, 2, 3];
        let protected = client_srtp.protect_rtp(&rtp, 1, None).expect("protect RTP");
        assert_ne!(protected, rtp);
        assert_eq!(
            server_srtp
                .unprotect_rtp(&protected, false)
                .expect("unprotect RTP")
                .0,
            rtp
        );

        let rtcp = [0x80, 200, 0, 1, 0, 0, 0, 7];
        let protected = client_srtp.protect_rtcp(&rtcp, None).expect("protect RTCP");
        assert_ne!(protected, rtcp);
        assert_eq!(
            server_srtp
                .unprotect_rtcp(&protected)
                .expect("unprotect RTCP"),
            rtcp
        );

        let now = pair.now;
        pair.client
            .send_application_data(b"data channel", now)
            .expect("send DTLS application data");
        while let Some(packet) = pair.client.poll_packet() {
            pair.server
                .handle_packet(&packet, now)
                .expect("receive DTLS application data");
        }
        assert!(std::iter::from_fn(|| pair.server.poll_event()).any(
            |event| matches!(event, DtlsEvent::ApplicationData(data) if data == b"data channel")
        ));
    }

    #[test]
    fn automatic_dtls_prefers_13_aes_128_gcm_and_aead_srtp() {
        let mut pair = connect(DtlsVersion::Dtls13);
        assert_eq!(
            pair.client.protocol_version(),
            Some(ProtocolVersion::DTLS1_3)
        );
        assert_eq!(
            pair.server.protocol_version(),
            Some(ProtocolVersion::DTLS1_3)
        );
        assert_eq!(
            server_hello_cipher_suite(&pair.server_packets),
            Some(0x1301)
        );
        assert!(matches!(
            pair.profile,
            SrtpProfile::AEAD_AES_128_GCM | SrtpProfile::AEAD_AES_256_GCM
        ));
        assert_protected_traffic(&mut pair);
    }

    #[test]
    fn automatic_dtls_falls_back_to_12_within_the_handshake() {
        let mut pair = connect(DtlsVersion::Dtls12);
        assert_eq!(
            pair.client.protocol_version(),
            Some(ProtocolVersion::DTLS1_2)
        );
        assert_eq!(
            pair.server.protocol_version(),
            Some(ProtocolVersion::DTLS1_2)
        );
        assert_protected_traffic(&mut pair);
    }
}
