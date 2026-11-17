//! Test-only access to real QUIC DATAGRAMs without application feedback or CC.

use crate::{Error, QuicPeer, QuicStats, Time, backend::Output, quic::Quic, wire};
use std::time::{Duration, Instant};

/// Matches the service's data framing and one-record staging, but creates no
/// application controller, receipt ledger, feedback generator or feedback lease.
pub struct QuicOnly {
    inner: Quic,
    origin: Instant,
    now: Time,
    ready: bool,
    drained: bool,
    pending: Option<Time>,
    attempt: u32,
    expired: u64,
}

impl QuicOnly {
    pub fn new(peer: QuicPeer, origin: Instant) -> Result<Self, Error> {
        let mut inner = Quic::new(&peer, origin)?;
        inner.handle_timeout(origin)?;
        Ok(Self {
            inner,
            origin,
            now: Time(0),
            ready: false,
            drained: false,
            pending: None,
            attempt: 0,
            expired: 0,
        })
    }

    pub fn ready(&self) -> bool {
        self.ready
    }

    pub fn input_ready(&self) -> bool {
        self.drained
    }

    pub fn expired(&self) -> u64 {
        self.expired
    }

    pub fn stats(&self) -> QuicStats {
        self.inner.stats()
    }

    pub fn receive(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if !self.drained {
            return Err(Error::Busy);
        }
        self.inner.handle_packet(bytes)?;
        self.drained = false;
        Ok(())
    }

    pub fn submit(&mut self, payload: &[u8], expires: Time) -> Result<(), Error> {
        if !self.ready || !self.drained {
            return Err(Error::NotReady);
        }
        if self.pending.is_some() {
            return Err(Error::Blocked);
        }
        if expires <= self.now {
            return Err(Error::Expired);
        }
        if payload.len() > crate::PAYLOAD_BUDGET {
            return Err(Error::Oversize);
        }
        let mut frame = vec![0; wire::DATA_HEADER];
        wire::data_header(&mut frame, [0; 4], self.attempt).map_err(|_| Error::Codec)?;
        frame.extend_from_slice(payload);
        self.inner.send_application_data(&frame)?;
        self.attempt = self.attempt.wrapping_add(1);
        self.pending = Some(expires);
        self.drained = false;
        Ok(())
    }

    pub fn poll(
        &mut self,
        now: Time,
        budget: usize,
        send: &mut dyn FnMut(&[u8]),
        receive: &mut dyn FnMut(&[u8]),
    ) -> Result<(), Error> {
        if now < self.now {
            return Err(Error::Time);
        }
        let instant = self
            .origin
            .checked_add(Duration::from_nanos(now.0))
            .ok_or(Error::Time)?;
        self.now = now;
        self.inner.handle_timeout(instant)?;
        self.drained = false;
        let mut buffer = [0; 2048];
        for _ in 0..budget {
            match self.inner.poll_output(&mut buffer)? {
                Output::Connected => self.ready = true,
                Output::Datagram(bytes) => {
                    let expired = self.inner.last_application()
                        && self.pending.take().is_some_and(|expires| expires <= now);
                    if expired {
                        self.expired = self.expired.saturating_add(1);
                    } else {
                        send(bytes);
                    }
                }
                Output::ApplicationData(bytes) => {
                    match wire::decode(bytes).map_err(|_| Error::Codec)? {
                        wire::Frame::Data { payload, .. } => receive(payload),
                        wire::Frame::Feedback { .. } => return Err(Error::Codec),
                    }
                }
                Output::Timeout(_) => {
                    self.drained = true;
                    return Ok(());
                }
                Output::Progress => {}
                Output::Closed => return Err(Error::Closed),
                Output::BufferTooSmall { .. } => return Err(Error::Capacity),
                Output::KeyingMaterial(_, _) => return Err(Error::Security),
            }
        }
        Ok(())
    }
}
