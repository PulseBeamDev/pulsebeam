//! Local transport receipts for windows after the route owner authorizes the
//! scope. The caller matches the complete wire ACK and child installation
//! before activating a receipt. No packet can manufacture a receipt.

use std::{
    collections::BTreeMap,
    num::{NonZeroU32, NonZeroU64},
    time::Instant,
};

use crate::dtls_srtp::wan::{
    Direction, MAX_DATAGRAM_BYTES, POSITION_MASK, RTP_TAG_BYTES,
    handoff::{Dropped, PrepareError, Prepared, SocketOutcome, Submission},
    rtp_header,
};

use super::{Binding, Error, Lease, Send, Session};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Purpose {
    Live,
    Repair { position: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowSpec {
    pub authority: NonZeroU64,
    pub direction: Direction,
    pub start: u64,
    pub ssrc: NonZeroU32,
    pub purpose: Purpose,
    pub expires: Instant,
}

impl WindowSpec {
    pub(super) fn admits(self, sequence: u16) -> Option<u64> {
        let position = (self.start & !0xffff) | u64::from(sequence);
        if position & !POSITION_MASK != self.start {
            return None;
        }
        match self.purpose {
            Purpose::Live => Some(position),
            Purpose::Repair { position: sought } if sought == position => Some(position),
            Purpose::Repair { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowTicket {
    lease: Lease,
    spec: WindowSpec,
}

impl WindowTicket {
    pub const fn spec(self) -> WindowSpec {
        self.spec
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowPhase {
    Staged,
    Active,
    FeedbackOnly,
}

struct WindowState {
    ticket: WindowTicket,
    phase: WindowPhase,
}

#[derive(Default)]
pub(super) struct Windows(BTreeMap<(Direction, NonZeroU32), WindowState>);

impl Windows {
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }
}

pub struct Opened {
    pub window: WindowTicket,
    pub position: u64,
    pub plaintext: Vec<u8>,
}

impl Session {
    /// Stage nonce/replay state for a specific receiver-issued label. No data
    /// is usable until the exact local ticket is activated by the owner.
    pub fn stage_window(
        &mut self,
        now: Instant,
        binding: Binding,
        spec: WindowSpec,
    ) -> Result<WindowTicket, Error> {
        if binding != self.binding || spec.expires <= now {
            return Err(Error::Window);
        }
        if let Purpose::Repair { position } = spec.purpose
            && position & !POSITION_MASK != spec.start
        {
            return Err(Error::Window);
        }
        self.advance(now)?;
        self.prune_windows()?;
        let lease = self.lease(now).ok_or(Error::Window)?;
        let ticket = WindowTicket { lease, spec };
        let key = (spec.direction, spec.ssrc);
        if let Some(state) = self.windows.0.get(&key) {
            return if state.ticket == ticket && state.phase != WindowPhase::FeedbackOnly {
                Ok(ticket)
            } else {
                Err(Error::Window)
            };
        }
        self.protector
            .as_mut()
            .ok_or(Error::Window)?
            .install(spec.direction, spec.ssrc, spec.start)
            .map_err(Error::Protection)?;
        self.windows.0.insert(
            key,
            WindowState {
                ticket,
                phase: WindowPhase::Staged,
            },
        );
        Ok(ticket)
    }

    pub fn activate_window(&mut self, now: Instant, ticket: WindowTicket) -> Result<(), Error> {
        self.advance(now)?;
        self.prune_windows()?;
        let state = self
            .windows
            .0
            .get_mut(&(ticket.spec.direction, ticket.spec.ssrc))
            .ok_or(Error::Window)?;
        if state.ticket != ticket || state.phase == WindowPhase::FeedbackOnly {
            return Err(Error::Window);
        }
        state.phase = WindowPhase::Active;
        Ok(())
    }

    pub fn retire_window(&mut self, ticket: WindowTicket) -> bool {
        let key = (ticket.spec.direction, ticket.spec.ssrc);
        let Some(state) = self
            .windows
            .0
            .get_mut(&key)
            .filter(|state| state.ticket == ticket && state.phase != WindowPhase::FeedbackOnly)
        else {
            return false;
        };
        if state.phase == WindowPhase::Active
            && matches!(ticket.spec.purpose, Purpose::Repair { .. })
        {
            // A one-shot repair can be freed by the application before its paced
            // RTCP receipt arrives. Preserve only feedback for media already sent.
            state.phase = WindowPhase::FeedbackOnly;
        } else {
            self.windows.0.remove(&key);
        }
        if ticket.spec.direction == Direction::Send {
            self.history.retire_send(ticket.spec.ssrc);
        }
        if let Some(protector) = &mut self.protector {
            protector.retire(ticket.spec.direction, ticket.spec.ssrc);
        }
        true
    }

    /// Path generation changes fence queued receipts and ciphertext. A new
    /// address or key epoch requires a fresh session instead.
    pub fn replace_path(&mut self, now: Instant, binding: Binding) -> bool {
        if self.binding.peer != binding.peer || self.advance(now).is_err() {
            return false;
        }
        if !self
            .path
            .as_mut()
            .is_some_and(|path| path.replace(now, binding))
        {
            return false;
        }
        self.binding = binding;
        self.incoming_owner = None;
        self.prune_windows().is_ok()
    }

    pub fn prepare_media(
        &mut self,
        now: Instant,
        ticket: WindowTicket,
        position: u64,
        plaintext: &[u8],
    ) -> Result<Prepared, Error> {
        self.advance(now)?;
        if ticket.spec.direction != Direction::Send
            || !self.active(ticket, now)
            || plaintext.len() > MAX_DATAGRAM_BYTES
        {
            return Err(Error::Window);
        }
        let header = rtp_header(plaintext, 0).map_err(|err| Error::Protection(err.into()))?;
        if ticket.spec.ssrc.get() != header.ssrc
            || ticket.spec.admits(header.sequence) != Some(position)
        {
            return Err(Error::Window);
        }
        let path = self.path.as_ref().ok_or(Error::Window)?;
        self.protector
            .as_mut()
            .ok_or(Error::Window)?
            .prepare_rtp(path, now, ticket.spec.expires, plaintext)
            .map_err(|error| match error {
                PrepareError::Stale => Error::Window,
                PrepareError::Seal(error) => Error::Protection(error),
            })
    }

    pub fn submit_media(
        &mut self,
        now: Instant,
        prepared: Prepared,
        send: &mut Send<'_>,
    ) -> Submission {
        if self.advance(now).is_err() || self.enforce_feedback_deadline(now).is_err() {
            return Submission::Dropped(Dropped::Stale);
        }
        let Some((ssrc, sequence)) = prepared.route() else {
            return Submission::Dropped(Dropped::Stale);
        };
        let Some(ssrc) = NonZeroU32::new(ssrc) else {
            return Submission::Dropped(Dropped::Stale);
        };
        let Some(route) = self.windows.0.get(&(Direction::Send, ssrc)) else {
            return Submission::Dropped(Dropped::Stale);
        };
        let ticket = route.ticket;
        let Some(position) = ticket.spec.admits(sequence) else {
            return Submission::Dropped(Dropped::Stale);
        };
        if !self.active(ticket, now) {
            return Submission::Dropped(Dropped::Stale);
        }
        let (Some(protector), Some(path)) = (&self.protector, &mut self.path) else {
            return Submission::Dropped(Dropped::Stale);
        };
        match self.history.ready(now, prepared.wire_len()) {
            pulsebeam_cc::Readiness::Ready => {}
            pulsebeam_cc::Readiness::Pacing { deadline } => {
                return Submission::Deferred {
                    prepared,
                    deadline: deadline.and_then(|at| self.history.instant(at)),
                };
            }
            pulsebeam_cc::Readiness::CongestionWindow => {
                return Submission::Deferred {
                    prepared,
                    deadline: None,
                };
            }
            _ => return Submission::Dropped(Dropped::Stale),
        }
        let Ok(commit) = self
            .history
            .prepare(now, ssrc, position, prepared.wire_len())
        else {
            return Submission::Dropped(Dropped::Failed);
        };
        let mut ect0 = false;
        let submission = protector.submit(path, now, prepared, |destination, payload| {
            let outcome = send(destination, payload);
            ect0 = matches!(outcome, SocketOutcome::AcceptedEct0 { .. });
            outcome
        });
        if matches!(submission, Submission::Accepted { .. })
            && self.history.accepted(now, commit, ect0).is_err()
        {
            // The socket receipt is still final even if accounting fails.
            self.close();
        } else {
            let _ = self.prune_windows();
        }
        submission
    }

    pub fn open_media(
        &mut self,
        now: Instant,
        binding: Binding,
        wire: &[u8],
    ) -> Result<Opened, Error> {
        self.open_media_with_ecn(now, binding, wire, None)
    }

    pub fn open_media_with_ecn(
        &mut self,
        now: Instant,
        binding: Binding,
        wire: &[u8],
        ecn: Option<super::Ecn>,
    ) -> Result<Opened, Error> {
        if binding != self.binding || wire.len() > MAX_DATAGRAM_BYTES {
            return Err(Error::Window);
        }
        self.advance(now)?;
        let header =
            rtp_header(wire, RTP_TAG_BYTES).map_err(|err| Error::Protection(err.into()))?;
        let ssrc = NonZeroU32::new(header.ssrc).ok_or(Error::Window)?;
        let window = self
            .windows
            .0
            .get(&(Direction::Receive, ssrc))
            .ok_or(Error::Window)?
            .ticket;
        if !self.active(window, now) {
            return Err(Error::Window);
        }
        let position = window.spec.admits(header.sequence).ok_or(Error::Window)?;
        let plaintext = self
            .protector
            .as_mut()
            .ok_or(Error::Window)?
            .open_rtp(wire)
            .map_err(Error::Protection)?;
        self.history.observe(now, ssrc, position, ecn);
        Ok(Opened {
            window,
            position,
            plaintext,
        })
    }

    pub(super) fn feedback_window(
        &self,
        direction: Direction,
        ssrc: NonZeroU32,
        now: Instant,
    ) -> Option<WindowSpec> {
        let state = self.windows.0.get(&(direction, ssrc))?;
        let ticket = state.ticket;
        (state.phase != WindowPhase::Staged
            && now < ticket.spec.expires
            && self.lease(now) == Some(ticket.lease))
        .then_some(ticket.spec)
    }

    fn active(&self, ticket: WindowTicket, now: Instant) -> bool {
        now < ticket.spec.expires
            && self.lease(now) == Some(ticket.lease)
            && self
                .windows
                .0
                .get(&(ticket.spec.direction, ticket.spec.ssrc))
                .is_some_and(|state| state.phase == WindowPhase::Active && state.ticket == ticket)
    }

    pub(super) fn prune_windows(&mut self) -> Result<(), Error> {
        let lease = self.lease(self.now);
        let now = self.now;
        if let Err(error) = self.history.sync(lease, now) {
            self.close();
            return Err(error);
        }
        let protector = &mut self.protector;
        let history = &mut self.history;
        self.windows.0.retain(|&(direction, ssrc), state| {
            if now < state.ticket.spec.expires && lease == Some(state.ticket.lease) {
                return true;
            }
            if direction == Direction::Send {
                history.retire_send(ssrc);
            }
            if let Some(protector) = protector {
                protector.retire(direction, ssrc);
            }
            false
        });
        Ok(())
    }
}
