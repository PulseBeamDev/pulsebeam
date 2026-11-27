//! Explicit Fabric WAN bootstrap. This owner never tries another carrier after
//! a handshake or capability failure. Route installation is separate.

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use pulsebeam_dtls::{Association, Output, SINGLE_RECORD_OVERHEAD};
pub use pulsebeam_dtls::{Fingerprint, LocalIdentity, Role};
pub use pulsebeam_feedback::Ecn;

use crate::fabric_wan::Capability;

mod history;
mod routes;
pub use routes::{LiveDeadline, Opened, Purpose, WindowSpec, WindowTicket};

use super::{
    Limits, Protector,
    handoff::SocketOutcome,
    path::{Binding, Lease, Path, Probe, Receipt},
};

const RETRY: Duration = Duration::from_millis(100);
const CAPABILITY_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUTS: usize = 64;
const POLL_BYTES: usize = 16 * 1024;
const OWNER_CONTROL_BYTES: usize = 1024;

#[derive(Debug)]
pub enum Error {
    Dtls(pulsebeam_dtls::Error),
    Protection(super::Error),
    Incompatible,
    Window,
    Control,
    FeedbackTimeout,
    Time,
    Capacity,
    Closed,
}

impl From<pulsebeam_dtls::Error> for Error {
    fn from(error: pulsebeam_dtls::Error) -> Self {
        Self::Dtls(error)
    }
}

#[derive(Clone, Copy)]
enum Emission {
    Offer,
    Accept,
    Receipt,
    Owner,
    Probe(Probe),
}

type Send<'a> = dyn FnMut(SocketAddr, &[u8]) -> SocketOutcome + 'a;

/// Feedback-qualified rates for one outbound path, in bits per second.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendBudget {
    pub feedback_at: Instant,
    pub valid_until: Instant,
    pub payload_bps: u64,
    pub pacing_bps: u64,
}

pub struct Session {
    binding: Binding,
    role: Role,
    bootstrap_mtu: usize,
    dtls: Association,
    limits: Limits,
    protector: Option<Protector>,
    path: Option<Path>,
    windows: routes::Windows,
    history: history::History,
    now: Instant,
    dtls_deadline: Option<Instant>,
    capability_deadline: Option<Instant>,
    offer_at: Option<Instant>,
    offer_attempted: bool,
    accept_pending: bool,
    receipt: Option<Receipt>,
    incoming_owner: Option<Box<[u8]>>,
    owner_submission: Option<SocketOutcome>,
    buffer: Vec<u8>,
    closed: bool,
}

pub struct Config<'a> {
    pub binding: Binding,
    pub role: Role,
    pub local: &'a LocalIdentity,
    pub peer: Fingerprint,
    pub bootstrap_mtu: usize,
    pub limits: Limits,
}

impl Session {
    pub fn new(config: Config<'_>, now: Instant) -> Result<Self, Error> {
        Ok(Self {
            binding: config.binding,
            role: config.role,
            bootstrap_mtu: config.bootstrap_mtu,
            dtls: Association::fabric_wan(
                config.role,
                config.local,
                config.peer,
                config.bootstrap_mtu,
                now,
            )?,
            limits: config.limits,
            protector: None,
            path: None,
            windows: routes::Windows::default(),
            history: history::History::new(config.binding, now)?,
            now,
            dtls_deadline: Some(now),
            capability_deadline: None,
            offer_at: None,
            offer_attempted: false,
            accept_pending: false,
            receipt: None,
            incoming_owner: None,
            owner_submission: None,
            buffer: vec![0; POLL_BYTES],
            closed: false,
        })
    }

    pub fn selected(&self) -> bool {
        self.path.is_some() && !self.closed
    }

    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn lease(&self, now: Instant) -> Option<Lease> {
        if now < self.now {
            return None;
        }
        self.path.as_ref()?.lease(now)
    }

    pub fn send_budget(&self, now: Instant) -> Option<SendBudget> {
        self.history.send_budget(now, self.lease(now)?)
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        if self.closed {
            return None;
        }
        self.dtls_deadline
            .into_iter()
            .chain(self.capability_deadline)
            .chain(self.offer_at)
            .chain(self.path.as_ref().and_then(Path::next_probe_at))
            .chain(self.path.as_ref().and_then(Path::qualification_expires_at))
            .chain(self.history.deadline(self.now))
            .min()
    }

    pub fn tick(&mut self, now: Instant, send: &mut Send<'_>) -> Result<(), Error> {
        self.advance(now)?;
        let result = self.run(None, send);
        self.finish(result)?;
        let result = self.flush_feedback(send);
        if result.is_err() {
            self.close();
        }
        result
    }

    /// Feed DTLS on the explicitly staged path. Other paths do not get to
    /// authenticate control or move the local protocol clock.
    pub fn receive(
        &mut self,
        now: Instant,
        binding: Binding,
        wire: &[u8],
        send: &mut Send<'_>,
    ) -> Result<bool, Error> {
        if binding != self.binding {
            return Ok(false);
        }
        self.advance(now)?;
        let result = self.run(Some(wire), send);
        self.finish(result).map(|()| true)
    }

    /// Send one bounded ownership frame on the selected association. This
    /// does not bypass path admission for either route activation or media.
    pub fn send_control(
        &mut self,
        now: Instant,
        bytes: &[u8],
        send: &mut Send<'_>,
    ) -> Result<SocketOutcome, Error> {
        self.advance(now)?;
        if !self.selected() || !owner_control(bytes) {
            return Err(Error::Control);
        }
        match self.dtls.send_application_data(bytes) {
            Err(pulsebeam_dtls::Error::DatagramBudget) => return Err(Error::Control),
            Err(error) => {
                self.close();
                return Err(error.into());
            }
            Ok(()) => {}
        }
        self.owner_submission = None;
        let result = self.drain(Some(Emission::Owner), send);
        self.finish(result)?;
        self.owner_submission.take().ok_or(Error::Control)
    }

    pub fn take_control(&mut self) -> Option<Box<[u8]>> {
        self.incoming_owner.take()
    }

    fn advance(&mut self, now: Instant) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if now < self.now {
            return Err(Error::Time);
        }
        self.now = now;
        Ok(())
    }

    fn close(&mut self) {
        self.closed = true;
        self.dtls.close();
        self.protector = None;
        self.path = None;
        self.windows.clear();
        let _ = self.history.sync(None, self.now);
        self.incoming_owner = None;
    }

    fn finish(&mut self, result: Result<(), Error>) -> Result<(), Error> {
        let result = result.and_then(|()| {
            self.prune_windows()?;
            self.history.poll(self.now)
        });
        if result.is_err() {
            self.close();
        }
        result
    }

    fn run(&mut self, input: Option<&[u8]>, send: &mut Send<'_>) -> Result<(), Error> {
        self.enforce_feedback_deadline(self.now)?;
        if self
            .capability_deadline
            .is_some_and(|deadline| self.now >= deadline)
        {
            return Err(Error::Incompatible);
        }
        self.dtls.handle_timeout(self.now)?;
        self.drain(None, send)?;
        if let Some(input) = input {
            match self.dtls.handle_packet(input) {
                // A rejected record must not displace a valid path lease.
                Err(pulsebeam_dtls::Error::DatagramBudget) => return Ok(()),
                Err(error) => return Err(error.into()),
                Ok(()) => self.drain(None, send)?,
            }
        }
        if std::mem::take(&mut self.accept_pending) {
            self.dtls
                .send_application_data(&Capability::Accept.encode())?;
            self.drain(Some(Emission::Accept), send)?;
        }
        if self.offer_at.is_some_and(|at| self.now >= at) {
            self.offer_at = Some(self.now.checked_add(RETRY).ok_or(Error::Time)?);
            self.dtls
                .send_application_data(&Capability::Offer.encode())?;
            self.drain(Some(Emission::Offer), send)?;
        }
        if let Some(receipt) = self.receipt.take() {
            self.dtls.send_application_data(&receipt.encode())?;
            self.drain(Some(Emission::Receipt), send)?;
        }
        if let Some(probe) = self
            .path
            .as_mut()
            .and_then(|path| path.begin_probe(self.now))
        {
            self.dtls.send_fabric_wan_probe(&probe.encode())?;
            self.drain(Some(Emission::Probe(probe)), send)?;
        }
        Ok(())
    }

    fn drain(&mut self, mut emission: Option<Emission>, send: &mut Send<'_>) -> Result<(), Error> {
        for _ in 0..MAX_OUTPUTS {
            match self.dtls.poll_output(&mut self.buffer)? {
                Output::Datagram(wire) => {
                    let count = self.dtls.last_output_application_records();
                    let purpose = if count == 0 {
                        None
                    } else {
                        Some(emission.take().ok_or(Error::Incompatible)?)
                    };
                    let size = wire.len();
                    let outcome = send(self.binding.peer, wire);
                    if outcome == SocketOutcome::Mtu
                        && let Some(path) = &mut self.path
                    {
                        path.fail_mtu(self.now, self.binding);
                    }
                    if let Some(purpose) = purpose {
                        match purpose {
                            Emission::Offer => {
                                // A later authenticated answer can resolve an ambiguous local submit.
                                self.offer_attempted |= matches!(outcome, SocketOutcome::Accepted { bytes } if bytes == size)
                                    || matches!(outcome, SocketOutcome::Unknown);
                            }
                            // A client must receive this answer and prove the full envelope
                            // before the server can get a directional lease of its own.
                            Emission::Accept => self.confirm(),
                            Emission::Probe(probe) => {
                                if let (Some(path), SocketOutcome::Accepted { bytes }) =
                                    (&mut self.path, outcome)
                                {
                                    path.probe_submitted(self.now, probe, bytes);
                                }
                            }
                            Emission::Receipt => {}
                            Emission::Owner => {
                                self.owner_submission = Some(match outcome {
                                    SocketOutcome::Accepted { bytes } if bytes != size => {
                                        SocketOutcome::Unknown
                                    }
                                    _ => outcome,
                                });
                            }
                        }
                    }
                }
                Output::ApplicationData(bytes) => {
                    let wire = self
                        .dtls
                        .last_wan_application_wire_bytes()
                        .ok_or(Error::Incompatible)?;
                    let capability = Capability::decode(bytes);
                    if let Some(capability) = capability {
                        if self.protector.is_none() || wire != 6 + SINGLE_RECORD_OVERHEAD {
                            return Err(Error::Incompatible);
                        }
                        match (self.role, capability) {
                            (Role::Server, Capability::Offer) => self.accept_pending = true,
                            (Role::Client, Capability::Accept) if self.offer_attempted => {
                                self.confirm();
                            }
                            _ => return Err(Error::Incompatible),
                        }
                    } else if let Some(path) = &mut self.path {
                        if owner_control(bytes) {
                            if self.incoming_owner.is_none() && wire <= self.bootstrap_mtu {
                                self.incoming_owner = Some(bytes.into());
                            }
                        } else {
                            self.receipt = path
                                .receive(self.now, self.binding, bytes, wire)
                                .or(self.receipt);
                        }
                    } else {
                        return Err(Error::Incompatible);
                    }
                }
                Output::KeyingMaterial(material, profile) => {
                    if self.protector.is_some() {
                        return Err(Error::Incompatible);
                    }
                    self.protector = Some(
                        Protector::from_dtls_exporter(material, profile, self.role, self.limits)
                            .map_err(Error::Protection)?,
                    );
                    self.capability_deadline = Some(
                        self.now
                            .checked_add(CAPABILITY_TIMEOUT)
                            .ok_or(Error::Time)?,
                    );
                    if self.role == Role::Client {
                        self.offer_at = Some(self.now);
                    }
                }
                Output::Timeout(deadline) => {
                    if emission.is_some() {
                        return Err(Error::Incompatible);
                    }
                    self.dtls_deadline = Some(deadline);
                    return Ok(());
                }
                Output::Progress | Output::Connected => {}
                Output::BufferTooSmall { .. } => return Err(Error::Capacity),
                Output::Closed => return Err(Error::Closed),
            }
        }
        Err(Error::Capacity)
    }

    fn confirm(&mut self) {
        if self.path.is_none() {
            self.path = Some(Path::after_accept(self.binding, self.role, self.now));
        }
        self.capability_deadline = None;
        self.offer_at = None;
    }
}

fn owner_control(bytes: &[u8]) -> bool {
    bytes.len() <= OWNER_CONTROL_BYTES && matches!(bytes, [1, _, ..])
}

#[cfg(test)]
mod tests;
