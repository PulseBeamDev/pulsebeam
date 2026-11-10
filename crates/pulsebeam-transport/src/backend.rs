//! Secure-datagram engines. No application routing, feedback or outer CC here.

use crate::quic::Quic;
use crate::{Error, PeerConfig};
use std::time::Instant;

pub(crate) use pulsebeam_dtls::Output;

pub(crate) enum Engine {
    Dtls(Box<pulsebeam_dtls::Association>),
    Quic(Box<Quic>),
}

impl Engine {
    pub(crate) fn new(peer: &PeerConfig, now: Instant, limit: Option<u64>) -> Result<Self, Error> {
        match peer {
            PeerConfig::Dtls(peer) => {
                #[cfg(any(test, feature = "test-utils"))]
                if let Some(limit) = limit {
                    return peer
                        .association_with_test_key_limit(now, limit)
                        .map(|engine| Self::Dtls(Box::new(engine)));
                }
                let _ = limit;
                peer.association(now)
                    .map(|engine| Self::Dtls(Box::new(engine)))
            }
            PeerConfig::Quic(peer) => {
                Quic::new(peer, now).map(|engine| Self::Quic(Box::new(engine)))
            }
        }
    }

    pub(crate) fn set_time(&mut self, now: Instant) {
        if let Self::Quic(engine) = self {
            engine.set_time(now);
        }
    }

    pub(crate) fn handle_packet(&mut self, packet: &[u8]) -> Result<(), Error> {
        match self {
            Self::Dtls(engine) => engine.handle_packet(packet).map_err(|_| Error::Security),
            Self::Quic(engine) => engine.handle_packet(packet),
        }
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) -> Result<(), Error> {
        match self {
            Self::Dtls(engine) => engine.handle_timeout(now).map_err(|_| Error::Security),
            Self::Quic(engine) => engine.handle_timeout(now),
        }
    }

    pub(crate) fn send_application_data(&mut self, bytes: &[u8]) -> Result<(), Error> {
        match self {
            Self::Dtls(engine) => engine
                .send_application_data(bytes)
                .map_err(|_| Error::Security),
            Self::Quic(engine) => engine.send_application_data(bytes),
        }
    }

    pub(crate) fn poll_output<'a>(&mut self, buffer: &'a mut [u8]) -> Result<Output<'a>, Error> {
        match self {
            Self::Dtls(engine) => engine.poll_output(buffer).map_err(|_| Error::Security),
            Self::Quic(engine) => engine.poll_output(buffer),
        }
    }

    pub(crate) fn last_output_application_records(&self) -> usize {
        match self {
            Self::Dtls(engine) => engine.last_output_application_records(),
            Self::Quic(engine) => usize::from(engine.last_application()),
        }
    }

    pub(crate) fn payload_capacity(&self) -> usize {
        match self {
            Self::Dtls(_) => crate::PAYLOAD_BUDGET,
            Self::Quic(engine) => engine
                .frame_capacity()
                .saturating_sub(crate::wire::DATA_HEADER)
                .min(crate::PAYLOAD_BUDGET),
        }
    }

    pub(crate) fn quic_stats(&self) -> Option<crate::QuicStats> {
        match self {
            Self::Dtls(_) => None,
            Self::Quic(engine) => Some(engine.stats()),
        }
    }

    pub(crate) fn close(&mut self) {
        match self {
            Self::Dtls(engine) => engine.close(),
            Self::Quic(engine) => engine.close(),
        }
    }
}
