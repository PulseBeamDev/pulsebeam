use std::{collections::BTreeMap, num::NonZeroU64, time::Duration};

use pulsebeam_runtime::time::Instant;

use super::{
    Member, RegionTree,
    frames::{Frame, HOPS, Request},
};

const MAX_FLIGHTS: usize = 64;
// A routed flight includes both directions and the owner's shard sampling.
pub(super) const ROUTED_READ_TTL: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Link {
    pub(super) member: Member,
    pub(super) stream: NonZeroU64,
}

#[derive(Clone, Copy)]
pub(super) struct Ticket {
    link: Link,
    id: NonZeroU64,
    expires: Instant,
}

impl Ticket {
    pub(super) fn live(self, now: Instant) -> bool {
        now < self.expires
    }
}

#[derive(Clone, Copy)]
enum Return {
    Local(Member),
    Remote(Ticket),
}

struct Flight {
    next: Link,
    origin: Return,
    expires: Instant,
}

pub(super) enum Arrival<'a> {
    Request(Ticket, &'a [u8]),
    Reply(Member, &'a [u8]),
    Branch(Request<'a>),
}

/// Forwarding state is per request, not per room, with one bounded lease per
/// forwarded flight. Replies traverse the exact streams that carried requests.
#[derive(Default)]
pub(super) struct Forwarding {
    flights: BTreeMap<NonZeroU64, Flight>,
    serial: u64,
}

pub(super) trait Links {
    fn first_local_hop(&self, destination: Member) -> Option<Member>;
    fn link(&self, to: Member) -> Option<Link>;
    fn send(&self, link: Link, bytes: Box<[u8]>, expires: Instant) -> bool;
}

pub(super) struct Router<'a, L> {
    pub(super) local: Member,
    pub(super) tree: &'a RegionTree,
    pub(super) links: L,
}

impl<L: Links> Router<'_, L> {
    fn request(
        &self,
        id: NonZeroU64,
        request: Request<'_>,
        incoming: Option<Member>,
        expires: Instant,
    ) -> Option<Link> {
        self.via(request, |hop, request| {
            if Some(hop) == incoming {
                return None;
            }
            let link = self.links.link(hop)?;
            self.links
                .send(link, Frame::Request(id, request).encode()?, expires)
                .then_some(link)
        })
    }

    pub(super) fn via<'p, T>(
        &self,
        request: Request<'p>,
        mut select: impl FnMut(Member, Request<'p>) -> Option<T>,
    ) -> Option<T> {
        if request.budget == 0 {
            return None;
        }
        let mut allowed = |hop, request| {
            self.tree.adjacency(self.local, hop)?;
            select(hop, request)
        };
        let local_region = self.tree.region(self.local)?;
        let target_region = self.tree.region(request.target)?;
        if local_region == target_region {
            let hop = self.links.first_local_hop(request.target)?;
            return allowed(hop, request);
        }
        let next_region = self.tree.next_region(local_region, target_region)?;
        let pairs: Vec<_> = self.tree.border_pairs(local_region, next_region).collect();
        let start = usize::from(request.attempt).checked_rem(pairs.len())?;
        pairs
            .iter()
            .copied()
            .cycle()
            .skip(start)
            .take(pairs.len())
            .find_map(|pair| {
                if request.border.is_some_and(|known| known != pair) {
                    return None;
                }
                let hop = if pair.0 == self.local {
                    pair.1
                } else {
                    self.links.first_local_hop(pair.0)?
                };
                allowed(
                    hop,
                    Request {
                        border: Some(pair),
                        ..request
                    },
                )
            })
    }

    fn validate_ingress(&self, incoming: Member, request: &mut Request<'_>) -> Option<()> {
        let ingress = self.tree.adjacency(self.local, incoming)?;
        let target_region = self.tree.region(request.target)?;
        if ingress.is_border() {
            if request.border != Some((incoming, self.local))
                || self.tree.next_region(ingress.peer_region, target_region)
                    != Some(ingress.local_region)
            {
                return None;
            }
            request.border = None;
        } else if let Some(pair) = request.border {
            let next = self.tree.next_region(ingress.local_region, target_region)?;
            if !self
                .tree
                .border_pairs(ingress.local_region, next)
                .any(|allowed| allowed == pair)
            {
                return None;
            }
        } else if ingress.local_region != target_region {
            return None;
        }
        Some(())
    }

    pub(super) fn reply(&self, ticket: Ticket, body: &[u8], now: Instant) -> bool {
        ticket.live(now)
            && Frame::Reply(ticket.id, body)
                .encode()
                .is_some_and(|frame| self.links.send(ticket.link, frame, ticket.expires))
    }
}

impl Forwarding {
    fn reserve(&mut self, now: Instant) -> Option<NonZeroU64> {
        self.flights.retain(|_, flight| now < flight.expires);
        (self.flights.len() < MAX_FLIGHTS).then_some(())?;
        self.serial = self.serial.checked_add(1)?;
        NonZeroU64::new(self.serial)
    }

    pub(super) fn request(
        &mut self,
        router: &Router<'_, impl Links>,
        owner: Member,
        attempt: u8,
        payload: &[u8],
        now: Instant,
    ) -> Option<()> {
        let id = self.reserve(now)?;
        let expires = now.checked_add(ROUTED_READ_TTL)?;
        let next = router.request(
            id,
            Request {
                target: owner,
                border: None,
                budget: HOPS,
                attempt,
                payload,
            },
            None,
            expires,
        )?;
        self.flights.insert(
            id,
            Flight {
                next,
                origin: Return::Local(owner),
                expires,
            },
        );
        Some(())
    }

    pub(super) fn receive<'a>(
        &mut self,
        router: &Router<'_, impl Links>,
        from: Link,
        frame: &'a [u8],
        now: Instant,
    ) -> Option<Arrival<'a>> {
        if router.links.link(from.member) != Some(from) {
            return None;
        }
        match Frame::decode(frame)? {
            Frame::Branch(mut request) => {
                router.validate_ingress(from.member, &mut request)?;
                Some(Arrival::Branch(request))
            }
            Frame::Request(id, mut request) => {
                router.validate_ingress(from.member, &mut request)?;
                let ticket = Ticket {
                    link: from,
                    id,
                    expires: now.checked_add(ROUTED_READ_TTL)?,
                };
                if request.target == router.local {
                    return Some(Arrival::Request(ticket, request.payload));
                }
                if self.flights.values().any(|flight| {
                    now < flight.expires && matches!(flight.origin, Return::Remote(previous) if previous.link == from && previous.id == id)
                }) {
                    return None;
                }
                request.budget = request.budget.checked_sub(1)?;
                let outgoing = self.reserve(now)?;
                let next = router.request(outgoing, request, Some(from.member), ticket.expires)?;
                self.flights.insert(
                    outgoing,
                    Flight {
                        next,
                        origin: Return::Remote(ticket),
                        expires: ticket.expires,
                    },
                );
                None
            }
            Frame::Reply(id, payload) => {
                let flight = self.flights.get(&id)?;
                if now >= flight.expires || from != flight.next {
                    return None;
                }
                match self.flights.remove(&id)?.origin {
                    Return::Local(owner) => Some(Arrival::Reply(owner, payload)),
                    Return::Remote(ticket) => {
                        router.reply(ticket, payload, now);
                        None
                    }
                }
            }
        }
    }
}
