//! Final handoff for one datagram from a live WAN path lease. The host must
//! use a nonfragmenting socket and report the exact kernel submission outcome.

use std::{net::SocketAddr, time::Instant};

use super::{
    Error, MAX_DATAGRAM_BYTES, Protector,
    path::{Lease, Path},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketOutcome {
    Accepted {
        bytes: usize,
    },
    /// Exact submission with the ECT(0) codepoint requested at the socket.
    AcceptedEct0 {
        bytes: usize,
    },
    /// Definite zero submission and a retryable local condition, such as EAGAIN.
    WouldBlock,
    /// Definite path MTU failure, with no fragment transmitted.
    Mtu,
    /// Terminal failure with no submission.
    Failed,
    /// The host cannot prove whether the datagram was submitted.
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dropped {
    Stale,
    Failed,
    Uncertain,
    Mtu,
}

pub enum Submission {
    Accepted {
        bytes: usize,
    },
    Retry(Prepared),
    /// No socket attempt; wait for the pacer or fresh congestion feedback.
    Deferred {
        prepared: Prepared,
        deadline: Option<Instant>,
    },
    Dropped(Dropped),
}

/// An unsubmitted datagram under one exact path lease. Only a definite zero
/// handoff returns ownership for retry. Never re-seal it.
pub struct Prepared {
    payload: Vec<u8>,
    route: Option<(u32, u16)>,
    lease: Lease,
    expires: Instant,
}

impl Prepared {
    pub(super) fn route(&self) -> Option<(u32, u16)> {
        self.route
    }

    /// Exact UDP payload bytes that would be handed to the socket.
    pub fn wire_len(&self) -> usize {
        self.payload.len()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum PrepareError {
    Stale,
    Seal(Error),
}

impl From<Error> for PrepareError {
    fn from(error: Error) -> Self {
        Self::Seal(error)
    }
}

impl Protector {
    pub fn prepare_rtp(
        &mut self,
        path: &Path,
        now: Instant,
        expires: Instant,
        plaintext: &[u8],
    ) -> Result<Prepared, PrepareError> {
        let lease = current_lease(path, now, expires)?;
        let (payload, header) = self.seal_rtp_with_route(plaintext)?;
        Ok(Prepared {
            payload,
            route: Some((header.ssrc, header.sequence)),
            lease,
            expires,
        })
    }

    pub fn prepare_feedback(
        &mut self,
        path: &Path,
        now: Instant,
        expires: Instant,
        plaintext: &[u8],
    ) -> Result<Prepared, PrepareError> {
        let lease = current_lease(path, now, expires)?;
        let payload = self.seal_feedback(plaintext)?;
        Ok(Prepared {
            payload,
            route: None,
            lease,
            expires,
        })
    }

    pub fn submit(
        &self,
        path: &mut Path,
        now: Instant,
        prepared: Prepared,
        send: impl FnOnce(SocketAddr, &[u8]) -> SocketOutcome,
    ) -> Submission {
        if !path.advance(now)
            || current_lease(path, now, prepared.expires) != Ok(prepared.lease)
            || prepared
                .route
                .is_some_and(|(ssrc, _)| !self.send.entries.get(&ssrc).is_some_and(Option::is_some))
            || prepared.payload.len() > MAX_DATAGRAM_BYTES
        {
            return Submission::Dropped(Dropped::Stale);
        }
        let binding = path.binding();
        match send(binding.peer, &prepared.payload) {
            SocketOutcome::Accepted { bytes } | SocketOutcome::AcceptedEct0 { bytes }
                if bytes == prepared.payload.len() =>
            {
                Submission::Accepted { bytes }
            }
            SocketOutcome::WouldBlock => Submission::Retry(prepared),
            SocketOutcome::Mtu => {
                path.fail_mtu(now, binding);
                Submission::Dropped(Dropped::Mtu)
            }
            SocketOutcome::Failed => Submission::Dropped(Dropped::Failed),
            SocketOutcome::Unknown
            | SocketOutcome::Accepted { .. }
            | SocketOutcome::AcceptedEct0 { .. } => Submission::Dropped(Dropped::Uncertain),
        }
    }
}

fn current_lease(path: &Path, now: Instant, expires: Instant) -> Result<Lease, PrepareError> {
    if now >= expires {
        return Err(PrepareError::Stale);
    }
    path.lease(now).ok_or(PrepareError::Stale)
}
