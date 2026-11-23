//! A scripted receipt-fact transport for deterministic multi-controller tests.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
    sync::{Arc, Mutex},
};

use pulsebeam_core::{
    auth::{ApiPublicKey, ApiSigningKey},
    identity::NodeId,
};

use super::{
    ClaimRank,
    observations::Session,
    receipt::{Error, Issuer, LineageTrust, PeerTrust},
};
use crate::control::controller::peer_ownership::Control;
use crate::entity::{ConnectionId, EndpointKey};
use pulsebeam_runtime::time::Instant;

const FACT_CAPACITY: usize = 64;
type Lineage = (EndpointKey, ConnectionId);

#[derive(Clone, Default)]
struct Facts {
    signed_through: BTreeMap<Lineage, u64>,
    retired: BTreeSet<Lineage>,
    unavailable: bool,
}

#[derive(Default)]
struct PeerFacts {
    local: Facts,
    received: Option<(u64, Facts)>,
    control: Option<Control>,
    outgoing: Option<(Option<pulsebeam_link::NetworkId>, Session)>,
    revision: u64,
}

/// One peer in a two-node simulation. Issuer keys are pinned independently of
/// receipt-fact delivery; copying facts makes no change to controller claims.
#[derive(Clone)]
#[allow(
    clippy::disallowed_types,
    reason = "simulation-only bounded fact transport between controller turns"
)]
pub struct SimOwnershipPeer {
    index: usize,
    facts: Arc<Mutex<[PeerFacts; 2]>>,
}

impl SimOwnershipPeer {
    #[allow(
        clippy::disallowed_types,
        reason = "simulation-only bounded fact transport between controller turns"
    )]
    pub fn pair() -> (Self, Self) {
        let facts = Arc::new(Mutex::new([PeerFacts::default(), PeerFacts::default()]));
        (
            Self {
                index: 0,
                facts: facts.clone(),
            },
            Self { index: 1, facts },
        )
    }

    fn with_facts<T>(&self, f: impl FnOnce(&mut PeerFacts, &mut PeerFacts) -> T) -> T {
        let mut facts = self
            .facts
            .lock()
            .unwrap_or_else(|_| pulsebeam_runtime::fatal!("simulation peer facts lock poisoned"));
        let [first, second] = &mut *facts;
        if self.index == 0 {
            f(first, second)
        } else {
            f(second, first)
        }
    }

    pub fn deliver_receipt_facts(&self, fresh_through: u64) {
        self.with_facts(|local, remote| {
            remote.received = Some((fresh_through, local.local.clone()));
        });
    }

    pub(crate) fn attach(&self, control: Control) {
        self.with_facts(|local, remote| {
            local.control = Some(control);
            local.outgoing = None;
            remote.outgoing = None;
        });
    }

    pub async fn local_claim_count(&self) -> anyhow::Result<usize> {
        let control = self
            .with_facts(|state, _| state.control.clone())
            .ok_or_else(|| anyhow::anyhow!("controller is not ready"))?;
        Ok(control
            .read()
            .await
            .map_err(|error| anyhow::anyhow!("ownership peer control: {error:?}"))?
            .len())
    }

    pub async fn deliver_claim_snapshot(&self, fresh_until: Instant) -> anyhow::Result<()> {
        self.deliver_snapshot(None, fresh_until).await
    }

    pub async fn deliver_linked_snapshot(
        &self,
        receiver_ingress: pulsebeam_link::NetworkId,
        fresh_until: Instant,
    ) -> anyhow::Result<()> {
        self.deliver_snapshot(Some(receiver_ingress), fresh_until)
            .await
    }

    pub async fn relay_fabric_data(
        &self,
        publisher_link: pulsebeam_link::NetworkId,
        receive_link: pulsebeam_link::NetworkId,
        connection: crate::entity::ConnectionId,
        fresh_until: Instant,
    ) -> anyhow::Result<(crate::entity::PublicationKey, pulsebeam_link::NodeRouteKey)> {
        let (source, destination) =
            self.with_facts(|local, remote| (local.control.clone(), remote.control.clone()));
        let source = source.ok_or_else(|| anyhow::anyhow!("source controller is not ready"))?;
        let destination =
            destination.ok_or_else(|| anyhow::anyhow!("destination controller is not ready"))?;
        let map_error = |error| anyhow::anyhow!("ownership Fabric image: {error:?}");
        let (publication, anchors) = loop {
            anyhow::ensure!(Instant::now() < fresh_until, "publisher anchor unavailable");
            let mut anchors = source.sample_data().await.map_err(map_error)?;
            anchors.retain(|_, (publisher, _)| publisher.connection == connection);
            anyhow::ensure!(anchors.len() <= 1, "expected one simulation publisher");
            if let Some(publication) = anchors.keys().next().copied() {
                break (publication, anchors);
            }
            pulsebeam_runtime::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        self.deliver_linked_snapshot(receive_link, fresh_until)
            .await?;
        let ack = loop {
            anyhow::ensure!(
                Instant::now() < fresh_until,
                "Fabric receive was not acknowledged"
            );
            let routes = destination
                .fabric_image(receive_link, Box::default(), anchors.clone(), fresh_until)
                .await
                .map_err(map_error)?;
            if let Some(ack) = routes
                .into_iter()
                .find(|ack| ack.publication == publication)
            {
                break ack;
            }
            pulsebeam_runtime::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        source
            .fabric_image(
                publisher_link,
                Box::new([ack]),
                Default::default(),
                fresh_until,
            )
            .await
            .map_err(map_error)?;
        Ok((publication, ack.key))
    }

    async fn deliver_snapshot(
        &self,
        ingress: Option<pulsebeam_link::NetworkId>,
        fresh_until: Instant,
    ) -> anyhow::Result<()> {
        let (source, destination, revision, outgoing) = self.with_facts(|local, remote| {
            let source = local
                .control
                .clone()
                .ok_or_else(|| anyhow::anyhow!("source controller is not ready"))?;
            let destination = remote
                .control
                .clone()
                .ok_or_else(|| anyhow::anyhow!("destination controller is not ready"))?;
            let next = local
                .revision
                .checked_add(1)
                .and_then(NonZeroU64::new)
                .ok_or_else(|| anyhow::anyhow!("snapshot revision exhausted"))?;
            local.revision = next.get();
            anyhow::Ok((source, destination, next, local.outgoing))
        })?;
        let map_error = |error| anyhow::anyhow!("ownership peer control: {error:?}");
        let claims = source.read().await.map_err(map_error)?;
        let from = issuer(self.index);
        let session = match (outgoing, ingress) {
            (Some((network, session)), _) if network == ingress => session,
            (_, Some(network)) => destination
                .open_linked(from.node, from.incarnation, network)
                .await
                .map_err(map_error)?,
            _ => destination
                .open(from.node, from.incarnation)
                .await
                .map_err(map_error)?,
        };
        self.with_facts(|local, _| local.outgoing = Some((ingress, session)));
        destination
            .snapshot(session, revision, claims, fresh_until)
            .await
            .map_err(map_error)
    }

    pub(in crate::control) fn trust(self) -> Box<dyn LineageTrust> {
        Box::new(Trust {
            key: signing_key(self.index),
            peer: self,
        })
    }
}

struct Trust {
    key: ApiSigningKey,
    peer: SimOwnershipPeer,
}

fn signing_key(index: usize) -> ApiSigningKey {
    ApiSigningKey::from_seed([if index == 0 { 11 } else { 12 }; 32])
}

fn issuer(index: usize) -> Issuer {
    Issuer {
        trust_domain: [8; 16],
        node: NodeId::from_bytes([
            0,
            0,
            0,
            0,
            0,
            0,
            0x70,
            0,
            0x80,
            0,
            0,
            0,
            0,
            0,
            0,
            if index == 0 { 1 } else { 2 },
        ]),
        incarnation: NonZeroU64::MIN,
    }
}

impl PeerTrust for Trust {
    fn issuer(&self) -> Issuer {
        issuer(self.peer.index)
    }

    fn sign(&self, message: &[u8]) -> Option<[u8; 64]> {
        Some(self.key.sign_message(message))
    }

    fn eligible_key(&self, claimed: Issuer) -> Option<ApiPublicKey> {
        if claimed == self.issuer() {
            return Some(self.key.public_key());
        }
        let remote = usize::from(self.peer.index == 0);
        (claimed == issuer(remote)).then(|| signing_key(remote).public_key())
    }
}

impl LineageTrust for Trust {
    fn record_issuance(
        &mut self,
        endpoint: EndpointKey,
        rank: ClaimRank,
        expires_at: u64,
        _now: u64,
    ) -> Result<(), Error> {
        self.peer.with_facts(|state, _| {
            let lineage = (endpoint, rank.lineage());
            if state.retired(lineage) {
                return Err(Error::Retired);
            }
            let local = &mut state.local;
            if local.unavailable
                || (!local.signed_through.contains_key(&lineage)
                    && local.signed_through.len() == FACT_CAPACITY)
            {
                return Err(Error::NoAuthority);
            }
            local
                .signed_through
                .entry(lineage)
                .and_modify(|deadline| *deadline = (*deadline).max(expires_at))
                .or_insert(expires_at);
            Ok(())
        })
    }

    fn check_recovery(
        &self,
        issuer: Issuer,
        endpoint: EndpointKey,
        rank: ClaimRank,
        expires_at: u64,
        now: u64,
    ) -> Result<(), Error> {
        self.peer.with_facts(|state, _| {
            let lineage = (endpoint, rank.lineage());
            if state.retired(lineage) {
                return Err(Error::Retired);
            }
            let source = if issuer == self.issuer() {
                &state.local
            } else {
                state.received(now).ok_or(Error::NoAuthority)?
            };
            if state.local.unavailable
                || source.unavailable
                || !source
                    .signed_through
                    .get(&lineage)
                    .is_some_and(|deadline| *deadline >= expires_at)
            {
                return Err(Error::NoAuthority);
            }
            Ok(())
        })
    }

    fn retire(&mut self, endpoint: EndpointKey, rank: ClaimRank) {
        self.peer.with_facts(|state, _| {
            let local = &mut state.local;
            if local.retired.len() == FACT_CAPACITY {
                local.unavailable = true;
            } else {
                local.retired.insert((endpoint, rank.lineage()));
            }
        });
    }

    fn is_retired(&self, endpoint: EndpointKey, rank: ClaimRank) -> bool {
        self.peer
            .with_facts(|state, _| state.retired((endpoint, rank.lineage())))
    }
}

impl PeerFacts {
    fn received(&self, now: u64) -> Option<&Facts> {
        self.received
            .as_ref()
            .filter(|(deadline, _)| now <= *deadline)
            .map(|(_, facts)| facts)
    }

    fn retired(&self, lineage: Lineage) -> bool {
        self.local.retired.contains(&lineage)
            || self
                .received
                .as_ref()
                .is_some_and(|(_, facts)| facts.retired.contains(&lineage))
    }
}
