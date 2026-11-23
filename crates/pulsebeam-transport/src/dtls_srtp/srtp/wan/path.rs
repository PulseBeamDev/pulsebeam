//! Directional Fabric WAN path qualification after authenticated capability acceptance.

use std::{
    net::SocketAddr,
    num::NonZeroU64,
    time::{Duration, Instant},
};

use pulsebeam_dtls::{Role, SINGLE_RECORD_OVERHEAD};

use super::MAX_DATAGRAM_BYTES;

const INTERVAL: Duration = Duration::from_secs(2);
const LIFETIME: Duration = Duration::from_secs(6);
const PREFIX_BYTES: usize = 13;
const RECEIPT_WIRE_BYTES: usize = PREFIX_BYTES + SINGLE_RECORD_OVERHEAD;
const PROBE_BYTES: usize = MAX_DATAGRAM_BYTES - SINGLE_RECORD_OVERHEAD;
const ENVELOPE: [u8; 2] = 0x04c0_u16.to_be_bytes();

/// Local identity only. Epoch and generation are never taken from peer bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Binding {
    pub epoch: NonZeroU64,
    pub generation: NonZeroU64,
    pub peer: SocketAddr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Lease {
    binding: Binding,
    witness: NonZeroU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Probe {
    binding: Binding,
    id: NonZeroU64,
}

impl Probe {
    pub fn encode(self) -> [u8; PROBE_BYTES] {
        let mut output = [0; PROBE_BYTES];
        let (prefix_out, _) = output.split_at_mut(PREFIX_BYTES);
        prefix_out.copy_from_slice(&prefix(1, self.id));
        output
    }

    pub const fn binding(self) -> Binding {
        self.binding
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Receipt {
    binding: Binding,
    id: NonZeroU64,
}

impl Receipt {
    pub fn encode(self) -> [u8; PREFIX_BYTES] {
        prefix(2, self.id)
    }

    pub const fn binding(self) -> Binding {
        self.binding
    }
}

fn prefix(kind: u8, id: NonZeroU64) -> [u8; PREFIX_BYTES] {
    let [a, b, c, d, e, f, g, h] = id.get().to_be_bytes();
    let [hi, lo] = ENVELOPE;
    [3, 2, kind, a, b, c, d, e, f, g, h, hi, lo]
}

enum Control {
    Probe(NonZeroU64),
    Receipt(NonZeroU64),
}

impl Control {
    fn decode(bytes: &[u8], sealed_len: usize) -> Option<Self> {
        let [3, 2, kind, a, b, c, d, e, f, g, h, hi, lo, fill @ ..] = bytes else {
            return None;
        };
        if [*hi, *lo] != ENVELOPE {
            return None;
        }
        let id = NonZeroU64::new(u64::from_be_bytes([*a, *b, *c, *d, *e, *f, *g, *h]))?;
        match (*kind, sealed_len) {
            (1, MAX_DATAGRAM_BYTES)
                if bytes.len() == PROBE_BYTES && fill.iter().all(|byte| *byte == 0) =>
            {
                Some(Self::Probe(id))
            }
            (2, RECEIPT_WIRE_BYTES) if fill.is_empty() => Some(Self::Receipt(id)),
            _ => None,
        }
    }
}

struct Challenge {
    probe: Probe,
    deadline: Instant,
    sent: bool,
}

struct Qualified {
    lease: Lease,
    expires: Instant,
}

pub struct Path {
    binding: Binding,
    role: Role,
    now: Instant,
    next_id: Option<NonZeroU64>,
    next_probe: Instant,
    challenge: Option<Challenge>,
    qualified: Option<Qualified>,
    peer_probe: Option<NonZeroU64>,
    peer_ready: bool,
    next_receipt: Instant,
}

impl Path {
    pub fn after_accept(binding: Binding, role: Role, now: Instant) -> Self {
        Self {
            binding,
            role,
            now,
            next_id: Some(NonZeroU64::MIN),
            next_probe: now,
            challenge: None,
            qualified: None,
            peer_probe: None,
            peer_ready: false,
            next_receipt: now,
        }
    }

    pub const fn binding(&self) -> Binding {
        self.binding
    }

    pub fn next_probe_at(&self) -> Option<Instant> {
        (self.next_id.is_some() && (self.role == Role::Client || self.peer_ready))
            .then_some(self.next_probe)
    }

    pub fn qualification_expires_at(&self) -> Option<Instant> {
        self.qualified.as_ref().map(|q| q.expires)
    }

    pub fn lease(&self, now: Instant) -> Option<Lease> {
        let qualified = self.qualified.as_ref()?;
        (now >= self.now && now < qualified.expires).then_some(qualified.lease)
    }

    /// A route replacement is forward-only within this exporter epoch. Probe
    /// IDs and rate limits do not restart with the path.
    pub fn replace(&mut self, now: Instant, binding: Binding) -> bool {
        if binding.epoch != self.binding.epoch
            || binding.generation <= self.binding.generation
            || !self.advance(now)
        {
            return false;
        }
        self.binding = binding;
        self.challenge = None;
        self.qualified = None;
        self.peer_ready = false;
        true
    }

    pub fn fail_mtu(&mut self, now: Instant, binding: Binding) {
        if binding == self.binding && self.advance(now) {
            self.challenge = None;
            self.qualified = None;
        }
    }

    pub fn begin_probe(&mut self, now: Instant) -> Option<Probe> {
        if !self.advance(now)
            || now < self.next_probe
            || self.challenge.is_some()
            || (self.role == Role::Server && !self.peer_ready)
        {
            return None;
        }
        let probe = Probe {
            binding: self.binding,
            id: self.next_id?,
        };
        let deadline = now.checked_add(INTERVAL)?;
        self.next_id = probe.id.checked_add(1);
        self.next_probe = deadline;
        self.challenge = Some(Challenge {
            probe,
            deadline,
            sent: false,
        });
        Some(probe)
    }

    /// Only a complete socket handoff makes the challenge eligible for a receipt.
    pub fn probe_submitted(&mut self, now: Instant, probe: Probe, bytes: usize) -> bool {
        if probe.binding != self.binding || !self.advance(now) || bytes != MAX_DATAGRAM_BYTES {
            return false;
        }
        let Some(challenge) = self
            .challenge
            .as_mut()
            .filter(|c| c.probe == probe && !c.sent)
        else {
            return false;
        };
        challenge.sent = true;
        true
    }

    /// The owner supplies exact ingress provenance from the selected authenticated
    /// DTLS association and its latest single-record application output.
    pub fn receive(
        &mut self,
        now: Instant,
        binding: Binding,
        plaintext: &[u8],
        sealed_len: usize,
    ) -> Option<Receipt> {
        if self.binding != binding || !self.advance(now) {
            return None;
        }
        match Control::decode(plaintext, sealed_len)? {
            Control::Probe(id) => {
                if self
                    .peer_probe
                    .is_some_and(|highest| id < highest || (id == highest && !self.peer_ready))
                {
                    return None;
                }
                self.peer_probe = Some(id);
                self.peer_ready = true;
                if now < self.next_receipt {
                    return None;
                }
                self.next_receipt = now.checked_add(INTERVAL)?;
                Some(Receipt { binding, id })
            }
            Control::Receipt(id) => {
                let challenge = self.challenge.as_ref()?;
                if challenge.sent && id == challenge.probe.id {
                    let expires = now.checked_add(LIFETIME)?;
                    let lease = self.lease(now).unwrap_or(Lease {
                        binding,
                        witness: id,
                    });
                    self.qualified = Some(Qualified { lease, expires });
                    self.challenge = None;
                }
                None
            }
        }
    }

    pub(super) fn advance(&mut self, now: Instant) -> bool {
        if now < self.now {
            return false;
        }
        self.now = now;
        if self
            .challenge
            .as_ref()
            .is_some_and(|challenge| now >= challenge.deadline)
        {
            self.challenge = None;
        }
        if self
            .qualified
            .as_ref()
            .is_some_and(|qualified| now >= qualified.expires)
        {
            self.qualified = None;
        }
        true
    }
}

#[cfg(test)]
mod tests;
