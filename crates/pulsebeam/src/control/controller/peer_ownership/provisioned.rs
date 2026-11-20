//! Operator-provisioned point-to-point ownership observation over Link.
//! No address, certificate, or incarnation is learned from membership gossip.

use std::{num::NonZeroU64, time::Duration};

use pulsebeam_core::identity::NodeId;
use pulsebeam_link::{LinkTime, NetworkId, NetworkPeer, PeerControl};
use pulsebeam_runtime::time::Instant;

use super::{
    ControllerActor, Session,
    ownership::{
        receipt::{Authority, Issuer, epoch::Trust},
        wire,
    },
    transfer::{self, Frame},
};

const REFRESH: Duration = Duration::from_secs(3);
const REOPEN: Duration = Duration::from_secs(1);
const LIVENESS: Duration = Duration::from_secs(15);
const FRAME_TTL: Duration = Duration::from_millis(450);
const RECEIVE_BUDGET: usize = 16;

use crate::cluster::ProvisionedPeer;

pub(in crate::control) struct Peer {
    remote: (NodeId, NonZeroU64),
    transport: NetworkPeer,
    association: Option<Association>,
    next_step: Instant,
    last_revision: u64,
}

struct Association {
    network: NetworkId,
    lane: PeerControl,
    epoch: Instant,
    session: Option<Session>,
    reader: transfer::Reader,
    sender: transfer::Sender,
    refresh: Instant,
    live_until: Instant,
}

impl Peer {
    fn new(remote: (NodeId, NonZeroU64), transport: NetworkPeer, now: Instant) -> Self {
        Self {
            remote,
            transport,
            association: None,
            next_step: now,
            last_revision: 0,
        }
    }

    pub(in crate::control) fn deadline(&self) -> Instant {
        self.next_step
    }

    fn step(&mut self, actor: &mut ControllerActor, now: Instant) {
        if self
            .association
            .as_ref()
            .is_some_and(|assoc| assoc.lane.is_closed() || now >= assoc.live_until)
        {
            // The socket owner has already fenced read/write. Withdraw the
            // ownership view on the actor, without enqueueing a close command.
            if let Some(assoc) = self.association.take() {
                actor.close_link_peer(assoc.network);
            }
            self.next_step = now.checked_add(REOPEN).unwrap_or(now);
        }
        if now < self.next_step {
            return;
        }
        self.next_step = now.checked_add(transfer::RETRY).unwrap_or(now);
        if let Some(assoc) = &mut self.association {
            assoc.step(actor, self.remote, &mut self.last_revision, now);
        } else if let Some(links) = &mut actor.links {
            if let Ok((network, lane)) = links.open_control_peer(self.transport.clone()) {
                self.association = Some(Association {
                    network,
                    lane,
                    epoch: links.clock_origin(),
                    session: None,
                    reader: transfer::Reader::default(),
                    sender: transfer::Sender::default(),
                    refresh: now,
                    live_until: now.checked_add(LIVENESS).unwrap_or(now),
                });
            } else {
                self.next_step = now.checked_add(REOPEN).unwrap_or(now);
            }
        }
    }
}

impl Association {
    fn step(
        &mut self,
        actor: &mut ControllerActor,
        remote: (NodeId, NonZeroU64),
        last_revision: &mut u64,
        now: Instant,
    ) {
        if !self.lane.is_ready() {
            return;
        }
        let session = if let Some(session) = self.session {
            session
        } else {
            let Ok(session) = actor.open_ownership_peer(remote.0, remote.1, Some(self.network))
            else {
                return;
            };
            self.session = Some(session);
            session
        };
        let Some(expires_at) = now.checked_add(FRAME_TTL).and_then(|at| {
            u64::try_from(at.saturating_duration_since(self.epoch).as_nanos())
                .ok()
                .map(LinkTime)
        }) else {
            return;
        };
        // Schedule the local pull before responding to the peer's bulk window.
        // Both directions remain bounded even when media or the peer is stalled.
        if now >= self.refresh && !self.reader.pending(now) {
            let _ = self.reader.start(now);
        }
        if let Some(frame) = self.reader.pull(now) {
            let _ = self.lane.try_send(frame, expires_at);
        }
        let mut sampled = false;
        for _ in 0..RECEIVE_BUDGET {
            let Some(bytes) = self.lane.try_recv() else {
                break;
            };
            match Frame::decode(&bytes) {
                Some(Frame::Pull(pull)) => {
                    if !sampled
                        && self.sender.needs_image(pull)
                        && let Some(revision) =
                            last_revision.checked_add(1).and_then(NonZeroU64::new)
                    {
                        sampled = true;
                        *last_revision = revision.get();
                        if let Some(Authority::Provisioned(trust)) = actor.receipt_trust.as_ref()
                            && let Ok(snapshot) = actor.local_endpoint_snapshot()
                            && let Ok(bytes) = wire::encode(revision, trust.checkpoint(), &snapshot)
                        {
                            self.sender.set_image(now, bytes);
                        }
                    }
                    for frame in self.sender.respond(now, pull) {
                        if self.lane.try_send(frame, expires_at).is_err() {
                            break;
                        }
                    }
                }
                Some(Frame::Chunk(chunk)) => {
                    if let Some((bytes, deadline)) = self.reader.receive(now, chunk)
                        && let Ok(image) = wire::decode(&bytes)
                        && self.lane.is_ready()
                        && actor
                            .apply_ownership_snapshot(
                                session,
                                image.revision,
                                image.endpoints,
                                deadline,
                                Some(image.authority),
                            )
                            .is_ok()
                    {
                        self.refresh = now.checked_add(REFRESH).unwrap_or(now);
                        self.live_until = now.checked_add(LIVENESS).unwrap_or(now);
                    }
                }
                None => {}
            }
        }
    }
}

impl ControllerActor {
    pub(crate) fn set_provisioned_peer(&mut self, spec: ProvisionedPeer) -> anyhow::Result<()> {
        let issuer = |(node, incarnation)| Issuer {
            trust_domain: spec.authority_epoch,
            node,
            incarnation,
        };
        let trust = Trust::new(
            issuer(spec.local),
            spec.receipt_key,
            issuer(spec.remote),
            spec.peer_receipt_key,
        )
        .map_err(|_| anyhow::anyhow!("invalid provisioned receipt authority"))?;
        self.receipt_trust = Some(Authority::Provisioned(Box::new(trust)));
        self.provisioned_peer = Some(Peer::new(spec.remote, spec.transport, Instant::now()));
        Ok(())
    }

    pub(in crate::control) fn drive_provisioned_peer(&mut self, now: Instant) {
        if let Some(mut peer) = self.provisioned_peer.take() {
            peer.step(self, now);
            self.provisioned_peer = Some(peer);
        }
    }
}
