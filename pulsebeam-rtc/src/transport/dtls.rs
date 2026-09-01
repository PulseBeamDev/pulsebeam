use std::collections::VecDeque;
use std::ops::RangeInclusive;
use std::time::Instant;

use str0m::crypto::dtls::{DtlsCert, DtlsOutput, DtlsVersion, KeyingMaterial, SrtpProfile};
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
        let mut instance = provider
            .dtls_provider
            .new_dtls(
                &certificate,
                now,
                DtlsVersion::Dtls12,
                Some(*DTLS_MTU.start()),
            )
            .map_err(|_| DtlsError::Crypto)?;
        instance.set_active(active);
        instance
            .handle_timeout(now)
            .map_err(|_| DtlsError::Crypto)?;
        let mut layer = Self {
            instance,
            remote_fingerprint,
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
