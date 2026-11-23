use crate::{PeerRole, Time, wire::Capability};

const RETRY_NS: u64 = 100_000_000;
const TIMEOUT_NS: u64 = 2_000_000_000;

pub(crate) struct Negotiation {
    role: PeerRole,
    expires: Option<Time>,
    offer_due: Option<Time>,
    offered: bool,
    accept_pending: bool,
    ready: bool,
}

impl Negotiation {
    pub(crate) fn new(role: PeerRole) -> Self {
        Self {
            role,
            expires: None,
            offer_due: None,
            offered: false,
            accept_pending: false,
            ready: false,
        }
    }

    pub(crate) fn connected(&mut self, now: Time) {
        self.expires = Some(Time(now.0.saturating_add(TIMEOUT_NS)));
        self.offer_due = matches!(self.role, PeerRole::Initiator).then_some(now);
    }

    pub(crate) fn timed_out(&self, now: Time) -> bool {
        self.expires.is_some_and(|expires| expires <= now)
    }

    pub(crate) fn deadline(&self) -> Option<Time> {
        self.expires.into_iter().chain(self.offer_due).min()
    }

    pub(crate) fn next(&mut self, now: Time) -> Option<Capability> {
        if std::mem::take(&mut self.accept_pending) {
            return Some(Capability::Accept);
        }
        if self.offer_due.is_some_and(|at| at <= now) {
            self.offer_due = Some(Time(now.0.saturating_add(RETRY_NS)));
            return Some(Capability::Offer);
        }
        None
    }

    /// Returns true only on the transition to readiness.
    pub(crate) fn accepted_by_socket(&mut self, capability: Capability) -> bool {
        match capability {
            Capability::Offer => {
                self.offered = true;
                false
            }
            Capability::Accept => self.confirm(),
        }
    }

    /// Returns true only on the transition to readiness.
    pub(crate) fn receive(&mut self, capability: Capability) -> bool {
        match (self.role, capability) {
            (PeerRole::Responder, Capability::Offer) if self.expires.is_some() || self.ready => {
                self.accept_pending = true;
                false
            }
            (PeerRole::Initiator, Capability::Accept) if self.offered => self.confirm(),
            _ => false,
        }
    }

    fn confirm(&mut self) -> bool {
        self.expires = None;
        self.offer_due = None;
        !std::mem::replace(&mut self.ready, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_retries_without_reopening_confirmed_state() {
        let mut initiator = Negotiation::new(PeerRole::Initiator);
        let mut responder = Negotiation::new(PeerRole::Responder);
        let now = Time(0);
        initiator.connected(now);
        responder.connected(now);
        assert!(!initiator.receive(Capability::Accept));
        assert_eq!(initiator.next(now), Some(Capability::Offer));
        assert!(!initiator.accepted_by_socket(Capability::Offer));
        assert!(!responder.receive(Capability::Offer));
        assert_eq!(responder.next(now), Some(Capability::Accept));
        assert!(responder.accepted_by_socket(Capability::Accept));
        assert_eq!(initiator.next(now), None);
        let retry = Time(RETRY_NS);
        assert_eq!(initiator.deadline(), Some(retry));
        assert_eq!(initiator.next(retry), Some(Capability::Offer));
        assert!(!responder.receive(Capability::Offer));
        assert_eq!(responder.next(retry), Some(Capability::Accept));
        assert!(!responder.accepted_by_socket(Capability::Accept));
        assert!(initiator.receive(Capability::Accept));
        assert!(!initiator.receive(Capability::Accept));
        assert_eq!(initiator.deadline(), None);
        assert_eq!(responder.deadline(), None);
    }
}
