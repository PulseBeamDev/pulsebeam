//! Per-hop downstream demand. The signed row identifies the origin and source
//! window. The ACK is for the adjacent receiver, not for the origin or an
//! arbitrary downstream hop. No transit is installed by decoding this frame.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    num::NonZeroU32,
    time::{Duration, SystemTime},
};

use pulsebeam_link::NetworkId;
use pulsebeam_proto::state::Track;
use pulsebeam_runtime::time::Instant;
use serde::{Deserialize, Serialize};

use crate::control::{
    controller::{
        ControllerActor,
        peer_ownership::relay::{
            self,
            fabric::{WanLiveRouteAck, WireWanLiveRouteAck},
            wan_publisher::PublisherSends,
            wan_source::WanSourceWindow,
        },
    },
    native::track_descriptor,
};

use super::{
    Member, RegionTree,
    forwarding::Link,
    frames::{HOPS, Request},
    media_permit,
    media_read::Row,
};

mod protection;
pub(super) use protection::ProtectionPath;

const TTL: Duration = Duration::from_secs(1);
const MAX_BYTES: usize = 32 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    row: Row,
    ack: WireWanLiveRouteAck,
    visited: Vec<Member>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    protection: Option<ProtectionPath>,
}

impl Wire {
    fn encode(&self) -> Option<Box<[u8]>> {
        if !(1..=HOPS as usize).contains(&self.visited.len()) {
            return None;
        }
        let bytes = serde_json::to_vec(self).ok()?;
        (bytes.len() <= MAX_BYTES).then_some(bytes.into_boxed_slice())
    }
}

pub(super) fn encode(
    row: Row,
    ack: WanLiveRouteAck,
    visited: Vec<Member>,
    protection: Option<ProtectionPath>,
) -> Option<Box<[u8]>> {
    Wire {
        row,
        ack: ack.into(),
        visited,
        protection,
    }
    .encode()
}

// Renewals are JSON objects; zero distinguishes a compact exact-ACK withdrawal.
const WITHDRAW: u8 = 0;

pub(super) fn withdraw(ack: WanLiveRouteAck) -> Option<Box<[u8]>> {
    let mut bytes = vec![WITHDRAW];
    serde_json::to_writer(&mut bytes, &WireWanLiveRouteAck::from(ack)).ok()?;
    Some(bytes.into_boxed_slice())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Edge {
    pub(super) link: Link,
    pub(super) network: NetworkId,
}

pub(super) struct Lease {
    pub(super) source: WanSourceWindow,
    pub(super) descriptor: Track,
    pub(super) permit: Box<[u8]>,
    pub(super) ack: WanLiveRouteAck,
    pub(super) downstream: Edge,
    pub(super) visited: Vec<Member>,
    pub(super) border: Option<(Member, Member)>,
    pub(super) attempt: u8,
    pub(super) expires: Instant,
    pub(super) issued_ms: u64,
    wall_expiry_ms: u64,
    pub(super) permit_deadline: Instant,
    pub(super) protection: Option<ProtectionPath>,
}

impl Lease {
    fn verify(
        tree: &RegionTree,
        local: Member,
        downstream: Edge,
        request: Request<'_>,
        now: Instant,
        wall: SystemTime,
    ) -> Option<Self> {
        let Wire {
            row,
            ack,
            visited,
            protection,
        } = serde_json::from_slice(request.payload).ok()?;
        if visited.last().copied() != Some(downstream.link.member)
            || visited.len().checked_add(request.budget.into())? != usize::from(HOPS) + 1
            || !valid_path(tree, local, request.target, &visited)
        {
            return None;
        }
        let ack = ack.decode()?;
        if protection.as_ref().is_some_and(|path| {
            request.border.is_some() || !path.matches_hop(tree, local, request.target, &visited)
        }) {
            return None;
        }
        let descriptor = row.descriptor()?;
        let (endpoint, track) = media_permit::source_name(ack.room.project, &descriptor)?;
        let permit = media_permit::verify(
            tree,
            request.target,
            endpoint,
            track,
            &descriptor,
            &row.permit,
            wall,
        )?;
        if !permit.source.matches_ack(ack)
            || (protection.is_some() && !supports_protection(permit.source, &descriptor))
        {
            return None;
        }
        let ttl = Duration::from_millis(
            permit
                .expires_ms
                .checked_sub(media_permit::unix_millis(wall)?)?,
        );
        let permit_deadline = now.checked_add(ttl)?;
        Some(Self {
            source: permit.source,
            descriptor,
            permit: row.permit,
            ack,
            downstream,
            visited,
            border: request.border,
            attempt: request.attempt,
            expires: permit_deadline.min(now.checked_add(TTL)?),
            issued_ms: permit.issued_ms,
            wall_expiry_ms: permit.expires_ms,
            permit_deadline,
            protection,
        })
    }
}

pub(super) fn supports_protection(source: WanSourceWindow, descriptor: &Track) -> bool {
    let emission = source.publisher.emission;
    emission.profile() == pulsebeam_core::fabric::Profile::H264
        && emission.carries_video_dd()
        && matches!(&descriptor.kind, Some(pulsebeam_proto::state::track::Kind::Video(video)) if video.guarantees_video_dd)
}

fn valid_path(tree: &RegionTree, local: Member, origin: Member, visited: &[Member]) -> bool {
    let mut seen = BTreeSet::from([local, origin]);
    if !visited.iter().all(|&member| seen.insert(member)) {
        return false;
    }
    let Some(target_region) = tree.region(origin) else {
        return false;
    };
    visited
        .iter()
        .copied()
        .zip(visited.iter().copied().skip(1).chain([local]))
        .all(|(from, to)| {
            tree.adjacency(from, to).is_some_and(|edge| {
                !edge.is_border()
                    || tree.next_region(edge.local_region, target_region) == Some(edge.peer_region)
            })
        })
}

// Ordinary demand can aggregate to one receive; protected legs are keyed by
// the additional exact downstream window and never feed ordinary aggregation.
type BranchKey = (Member, Member, relay::Key, u64, Option<NonZeroU32>);

#[derive(Default)]
pub(super) struct Leases {
    inner: BTreeMap<BranchKey, Lease>,
}

impl Leases {
    pub(super) fn receive(
        &mut self,
        tree: &RegionTree,
        local: Member,
        from: Edge,
        request: Request<'_>,
        now: Instant,
        wall: SystemTime,
    ) {
        if request.payload.len() > MAX_BYTES {
            return;
        }
        if let Some(body) = request.payload.strip_prefix(&[WITHDRAW]) {
            if let Some(ack) = serde_json::from_slice::<WireWanLiveRouteAck>(body)
                .ok()
                .and_then(WireWanLiveRouteAck::decode)
            {
                for leg in [None, Some(ack.window.ssrc())] {
                    let key = (
                        from.link.member,
                        request.target,
                        relay::Key::new(ack.room, ack.publication, false),
                        ack.window.start(),
                        leg,
                    );
                    if self.inner.get(&key).is_some_and(|lease| {
                        lease.downstream.link == from.link
                            && lease.downstream.network == from.network
                            && lease.ack == ack
                    }) {
                        self.inner.remove(&key);
                    }
                }
            }
            return;
        }
        let Some(mut lease) = Lease::verify(tree, local, from, request, now, wall) else {
            return;
        };
        let key = (
            from.link.member,
            request.target,
            lease.source.scope(),
            lease.source.window_start,
            lease.protection.as_ref().map(|_| lease.ack.window.ssrc()),
        );
        if self.inner.iter().any(|(other, current)| {
            &key != other
                && (key.4.is_some() || other.4.is_some())
                && current.downstream.network == from.network
                && current.ack.window.ssrc() == lease.ack.window.ssrc()
        }) {
            return;
        }
        if let Some(current) = self.inner.get_mut(&key) {
            if lease.issued_ms < current.issued_ms
                || current.protection != lease.protection
                || (lease.protection.is_some()
                    && (current.ack != lease.ack
                        || current.downstream.link != from.link
                        || current.downstream.network != from.network))
            {
                return;
            }
            if lease.issued_ms == current.issued_ms
                && lease.wall_expiry_ms == current.wall_expiry_ms
            {
                lease.permit_deadline = lease.permit_deadline.min(current.permit_deadline);
                lease.expires = lease.expires.min(lease.permit_deadline);
            }
            *current = lease;
        } else if self.inner.len() < relay::LIMIT {
            self.inner.insert(key, lease);
        }
    }

    pub(super) fn expire(&mut self, now: Instant, wall: SystemTime) {
        let wall = media_permit::unix_millis(wall);
        self.inner.retain(|_, lease| {
            now < lease.expires && wall.is_some_and(|wall| wall < lease.wall_expiry_ms)
        });
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &Lease> {
        self.inner
            .values()
            .filter(|lease| lease.protection.is_none())
    }

    pub(super) fn protected(&self) -> impl Iterator<Item = &Lease> {
        self.inner
            .values()
            .filter(|lease| lease.protection.is_some())
    }
}

#[derive(Default)]
pub(super) struct OriginSends {
    peers: HashMap<NetworkId, PublisherSends>,
}

impl OriginSends {
    pub(super) fn live_media_grant(
        &self,
        peer: NetworkId,
        demand: super::super::peer_ownership::target_demand::Demand,
        now: Instant,
    ) -> Option<super::super::peer_ownership::media_repair::Grant> {
        self.peers
            .get(&peer)?
            .live_media_grant(peer, demand.label, demand.incarnation, now)
    }

    pub(super) fn media_repair_grant(
        &self,
        peer: NetworkId,
        demand: crate::control::controller::peer_ownership::media_repair::Demand,
        now: Instant,
    ) -> Option<crate::control::controller::peer_ownership::media_repair::Grant> {
        self.peers.get(&peer)?.media_repair_grant(peer, demand, now)
    }

    pub(super) fn step(
        &mut self,
        actor: &mut ControllerActor,
        local: Member,
        leases: &Leases,
        now: Instant,
        wall: SystemTime,
    ) {
        let Some(wall_ms) = media_permit::unix_millis(wall) else {
            return;
        };
        let mut desired = HashMap::<_, Vec<_>>::new();
        for lease in leases
            .iter()
            .chain(leases.protected())
            .filter(|lease| lease.source.origin == Some(local))
        {
            let Some((track, valid_until)) =
                media_permit::current_publisher(actor, lease.source, wall)
            else {
                continue;
            };
            if track_descriptor(track).as_ref() != Some(&lease.descriptor) {
                continue;
            }
            let Some(until) =
                now.checked_add(Duration::from_millis(valid_until.saturating_sub(wall_ms)))
            else {
                continue;
            };
            let wire_bps = lease.protection.as_ref().map(ProtectionPath::wire_bps);
            let expires = if wire_bps.is_some() {
                lease.expires
            } else {
                lease.permit_deadline
            }
            .min(until);
            desired.entry(lease.downstream.network).or_default().push((
                lease.source,
                lease.ack,
                expires,
                wire_bps,
            ));
        }
        for (network, sends) in &mut self.peers {
            sends.reconcile(
                actor,
                *network,
                desired.remove(network).unwrap_or_default(),
                now,
            );
        }
        self.peers.retain(|_, sends| !sends.is_empty());
        for (network, permits) in desired {
            let mut sends = PublisherSends::default();
            sends.reconcile(actor, network, permits, now);
            if !sends.is_empty() {
                self.peers.insert(network, sends);
            }
        }
    }
}
