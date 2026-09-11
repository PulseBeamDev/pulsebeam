use std::collections::VecDeque;
use std::ops::RangeInclusive;
use std::time::Instant;

use str0m::crypto::dtls::{
    DtlsCert, DtlsOutput, DtlsVersion, KeyingMaterial, ProtocolVersion, SrtpProfile,
};
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

pub(crate) struct DtlsLayer {
    instance: Box<dyn str0m::crypto::dtls::DtlsInstance>,
    remote_fingerprint: Fingerprint,
    negotiated_profile: Option<SrtpProfile>,
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
        provider: &CryptoProvider,
        version: DtlsVersion,
    ) -> Result<Self, DtlsError> {
        let mut instance = provider
            .dtls_provider
            .new_dtls(&certificate, now, version, Some(*DTLS_MTU.start()))
            .map_err(|_| DtlsError::Crypto)?;
        instance.set_active(active);
        instance
            .handle_timeout(now)
            .map_err(|_| DtlsError::Crypto)?;
        let mut layer = Self {
            instance,
            remote_fingerprint,
            negotiated_profile: None,
            connected: false,
            closing: false,
            closed: false,
            packets: VecDeque::new(),
            events: VecDeque::new(),
            poll_buffer: vec![0; DTLS_POLL_BUFFER],
            next_deadline: None,
        };
        layer.drive(now)?;
        Ok(layer)
    }

    pub(crate) fn connected(&self) -> bool {
        self.connected
    }

    pub(crate) fn protocol_version(&self) -> Option<ProtocolVersion> {
        self.instance.protocol_version()
    }

    pub(crate) fn negotiated_profile(&self) -> Option<SrtpProfile> {
        self.negotiated_profile
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.next_deadline
    }

    pub(crate) fn handle_packet(&mut self, packet: &[u8], now: Instant) -> Result<(), DtlsError> {
        let Some(&content_type) = packet.first() else {
            return Err(DtlsError::InvalidState);
        };
        if self.closed || self.closing || packet.len() < 13 || !(20..=64).contains(&content_type) {
            return Err(DtlsError::InvalidState);
        }
        self.instance
            .handle_packet(packet)
            .map_err(|_| DtlsError::Crypto)?;
        self.drive(now)
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) -> Result<(), DtlsError> {
        if self.closed || self.closing {
            return Ok(());
        }
        let Some(deadline) = self.next_deadline else {
            return Err(DtlsError::InvalidState);
        };
        if now < deadline {
            return Err(DtlsError::InvalidState);
        }
        self.instance
            .handle_timeout(now)
            .map_err(|_| DtlsError::Crypto)?;
        self.drive(now)
    }

    pub(crate) fn close(&mut self, now: Instant) -> Result<(), DtlsError> {
        if self.closed {
            return Ok(());
        }
        self.instance.close().map_err(|_| DtlsError::Crypto)?;
        self.closing = true;
        self.next_deadline = None;
        self.drive(now)
    }

    pub(crate) fn send_application_data(
        &mut self,
        data: &[u8],
        now: Instant,
    ) -> Result<(), DtlsError> {
        if !self.connected || self.closing || self.closed {
            return Err(DtlsError::InvalidState);
        }
        self.instance
            .send_application_data(data)
            .map_err(|_| DtlsError::Crypto)?;
        self.drive(now)
    }

    pub(crate) fn poll_packet(&mut self) -> Option<Vec<u8>> {
        let packet = self.packets.pop_front();
        debug_assert!(self.packets.len() <= MAX_OUTPUT);
        packet
    }

    pub(crate) fn poll_event(&mut self) -> Option<DtlsEvent> {
        self.events.pop_front()
    }

    pub(crate) fn clear_pending(&mut self) {
        self.packets.clear();
        self.events.clear();
    }

    fn drive(&mut self, now: Instant) -> Result<(), DtlsError> {
        let mut poll_buffer = std::mem::take(&mut self.poll_buffer);
        let result = self.drive_with_buffer(now, &mut poll_buffer);
        self.poll_buffer = poll_buffer;
        result
    }

    fn drive_with_buffer(
        &mut self,
        _now: Instant,
        poll_buffer: &mut [u8],
    ) -> Result<(), DtlsError> {
        for _ in 0..MAX_OUTPUT {
            let output = self.instance.poll_output(poll_buffer);
            match output {
                DtlsOutput::Packet(packet) => {
                    let packet = packet.to_vec();
                    self.enqueue_packet(&packet)?;
                }
                DtlsOutput::BufferTooSmall { .. } => return Err(DtlsError::BufferTooSmall),
                DtlsOutput::Timeout(deadline) => {
                    self.next_deadline = (!self.connected && !self.closing).then_some(deadline);
                    return Ok(());
                }
                DtlsOutput::Connected => {
                    if !self.connected {
                        self.connected = true;
                        self.enqueue_event(DtlsEvent::Connected)?;
                    }
                    self.next_deadline = None;
                }
                DtlsOutput::PeerCert(certificate) => {
                    let provider = str0m::crypto::from_feature_flags();
                    let actual = Fingerprint {
                        hash_func: "sha-256".to_owned(),
                        bytes: provider.sha256_provider.sha256(certificate).to_vec(),
                    };
                    if actual != self.remote_fingerprint {
                        return Err(DtlsError::FingerprintMismatch);
                    }
                }
                DtlsOutput::KeyingMaterial(material, profile) => {
                    self.negotiated_profile = Some(profile);
                    self.enqueue_event(DtlsEvent::KeyingMaterial(material, profile))?;
                }
                DtlsOutput::ApplicationData(data) => {
                    self.enqueue_event(DtlsEvent::ApplicationData(data.to_vec()))?;
                }
                DtlsOutput::CloseNotify => {
                    self.closing = true;
                    self.closed = true;
                    self.next_deadline = None;
                    self.enqueue_event(DtlsEvent::CloseNotify)?;
                    return Ok(());
                }
                _ => return Err(DtlsError::Crypto),
            }
        }
        Err(DtlsError::OutputFull)
    }

    fn enqueue_packet(&mut self, packet: &[u8]) -> Result<(), DtlsError> {
        if packet.is_empty() || packet.len() > *DTLS_MTU.end() || self.packets.len() >= MAX_OUTPUT {
            return Err(DtlsError::OutputFull);
        }
        self.packets.push_back(packet.to_vec());
        debug_assert!(self.packets.len() <= MAX_OUTPUT);
        Ok(())
    }

    fn enqueue_event(&mut self, event: DtlsEvent) -> Result<(), DtlsError> {
        if self.events.len() >= MAX_OUTPUT {
            return Err(DtlsError::OutputFull);
        }
        self.events.push_back(event);
        debug_assert!(self.events.len() <= MAX_OUTPUT);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::srtp::SrtpLayer;

    struct ConnectedPair {
        client: DtlsLayer,
        server: DtlsLayer,
        client_material: Option<KeyingMaterial>,
        server_material: Option<KeyingMaterial>,
        profile: SrtpProfile,
        server_packets: Vec<Vec<u8>>,
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
        let mut client_srtp =
            SrtpLayer::new(client_material, pair.profile, true, &provider).expect("client SRTP");
        let mut server_srtp =
            SrtpLayer::new(server_material, pair.profile, false, &provider).expect("server SRTP");
        let rtp = [0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7, 1, 2, 3];
        let protected = client_srtp.protect_rtp(&rtp).expect("protect RTP");
        assert_ne!(protected, rtp);
        assert_eq!(
            server_srtp
                .unprotect_rtp(&protected)
                .expect("unprotect RTP")
                .0,
            rtp
        );

        let rtcp = [0x80, 200, 0, 1, 0, 0, 0, 7];
        let protected = client_srtp.protect_rtcp(&rtcp).expect("protect RTCP");
        assert_ne!(protected, rtcp);
        assert_eq!(
            server_srtp
                .unprotect_rtcp(&protected)
                .expect("unprotect RTCP"),
            rtcp
        );

        let now = Instant::now();
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
