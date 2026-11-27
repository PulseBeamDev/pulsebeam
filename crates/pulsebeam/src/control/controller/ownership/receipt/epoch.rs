//! Receipt authority for a fixed peer set in one provisioned authority epoch.
//! Neither an expired view nor a Link reconnect forgets terminal lineage facts.

use std::{collections::BTreeMap, num::NonZeroU64};

use pulsebeam_core::identity::NodeId;
use pulsebeam_runtime::time::Instant;
use serde::{Deserialize, Serialize};

use super::{
    ApiPublicKey, ApiSigningKey, ClaimRank, ConnectionId, EndpointKey, Error, Issuer, LineageTrust,
    ParticipantId, PeerTrust, ProjectId, ProjectRoomKey, RoomId, SIGNATURE_BYTES,
};

const CAPACITY: usize = 64;
type Key = (EndpointKey, ConnectionId);
type Ledger = BTreeMap<Key, Fact>;

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Fact {
    issued_through: u64,
    retired: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    project: ProjectId,
    room: RoomId,
    participant: ParticipantId,
    root: ConnectionId,
    fact: Fact,
}

impl Record {
    fn key(&self) -> Key {
        (
            EndpointKey {
                room: ProjectRoomKey {
                    project: self.project,
                    room: self.room,
                },
                participant: self.participant,
            },
            self.root,
        )
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::control) struct Checkpoint {
    issuer: Issuer,
    blocked: bool,
    records: Box<[Record]>,
}

struct View {
    revision: NonZeroU64,
    deadline: Instant,
    blocked: bool,
    records: Ledger,
}

pub(in crate::control) struct Validated {
    issuer: Issuer,
    view: View,
    retired: Vec<Key>,
}

impl Validated {
    pub(in crate::control) fn blocked(&self) -> bool {
        self.view.blocked
    }
}

struct Remote {
    issuer: Issuer,
    key: ApiPublicKey,
    view: Option<View>,
}

pub(in crate::control) struct Trust {
    issuer: Issuer,
    key: ApiSigningKey,
    peers: BTreeMap<NodeId, Remote>,
    blocked: bool,
    records: Ledger,
}

impl Trust {
    pub(in crate::control) fn new(
        issuer: Issuer,
        key: ApiSigningKey,
        remote: Issuer,
        remote_key: ApiPublicKey,
    ) -> Result<Self, Error> {
        let mut trust = Self {
            issuer,
            key,
            peers: BTreeMap::new(),
            blocked: false,
            records: Ledger::new(),
        };
        trust.pin_peer(remote, remote_key)?;
        Ok(trust)
    }

    pub(in crate::control) fn matches_local(&self, issuer: Issuer, key: ApiPublicKey) -> bool {
        self.issuer == issuer && self.key.public_key() == key
    }

    pub(in crate::control) fn pin_peer(
        &mut self,
        issuer: Issuer,
        key: ApiPublicKey,
    ) -> Result<(), Error> {
        if issuer.trust_domain != self.issuer.trust_domain
            || issuer.node == self.issuer.node
            || key == self.key.public_key()
            || self.peers.contains_key(&issuer.node)
            || self.peers.values().any(|peer| peer.key == key)
        {
            return Err(Error::Invalid);
        }
        if self.peers.len() >= crate::link::host::PEERS {
            return Err(Error::NoAuthority);
        }
        self.peers.insert(
            issuer.node,
            Remote {
                issuer,
                key,
                view: None,
            },
        );
        Ok(())
    }

    /// Advance an enrolled slot only after authenticated, durable boot admission.
    /// Old issuance is not transferable; learned terminal facts are.
    pub(in crate::control) fn advance_peer(&mut self, issuer: Issuer) -> Result<(), Error> {
        let peer = self.peers.get_mut(&issuer.node).ok_or(Error::Invalid)?;
        if issuer.trust_domain != peer.issuer.trust_domain
            || issuer.incarnation <= peer.issuer.incarnation
        {
            return Err(Error::Invalid);
        }
        if let Some(view) = peer.view.take() {
            self.blocked |= view.blocked;
            for (key, fact) in view.records {
                if !fact.retired {
                    continue;
                }
                if self.records.len() >= CAPACITY && !self.records.contains_key(&key) {
                    self.blocked = true;
                    break;
                }
                self.records.entry(key).or_default().retired = true;
            }
        }
        peer.issuer = issuer;
        Ok(())
    }

    pub(in crate::control) fn pinned_origins(
        &self,
    ) -> impl Iterator<Item = (NodeId, NonZeroU64)> + '_ {
        self.peers
            .values()
            .map(|peer| (peer.issuer.node, peer.issuer.incarnation))
    }

    pub(in crate::control) fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            issuer: self.issuer,
            blocked: self.blocked,
            records: self
                .records
                .iter()
                .map(|(&(endpoint, root), &fact)| Record {
                    project: endpoint.room.project,
                    room: endpoint.room.room,
                    participant: endpoint.participant,
                    root,
                    fact,
                })
                .collect(),
        }
    }

    /// The caller binds this image to the current pinned association and the
    /// pull-start deadline before it drives admission or live source changes.
    pub(in crate::control) fn validate_checkpoint(
        &self,
        origin: (NodeId, NonZeroU64),
        revision: NonZeroU64,
        checkpoint: Checkpoint,
        now: Instant,
        deadline: Instant,
    ) -> Result<Validated, Error> {
        if now >= deadline {
            return Err(Error::Unfresh);
        }
        let remote = self.peers.get(&origin.0).ok_or(Error::Invalid)?;
        if remote.issuer.incarnation != origin.1
            || checkpoint.issuer != remote.issuer
            || checkpoint.records.len() > CAPACITY
        {
            return Err(Error::Invalid);
        }
        let mut records = Ledger::new();
        for record in checkpoint.records {
            if (record.fact.issued_through == 0 && !record.fact.retired)
                || records.insert(record.key(), record.fact).is_some()
            {
                return Err(Error::Invalid);
            }
        }
        if let Some(old) = &remote.view {
            if old.revision >= revision || old.blocked && !checkpoint.blocked {
                return Err(Error::Invalid);
            }
            if old.records.iter().any(|(key, old_fact)| {
                records.get(key).is_none_or(|new_fact| {
                    new_fact.issued_through < old_fact.issued_through
                        || (old_fact.retired && !new_fact.retired)
                })
            }) {
                return Err(Error::Invalid);
            }
        }
        let newly_retired = records
            .iter()
            .filter_map(|(key, fact)| (fact.retired && !self.retired(*key)).then_some(*key))
            .collect();
        Ok(Validated {
            issuer: remote.issuer,
            view: View {
                revision,
                deadline,
                blocked: checkpoint.blocked,
                records,
            },
            retired: newly_retired,
        })
    }

    pub(in crate::control) fn install_checkpoint(
        &mut self,
        checkpoint: Validated,
    ) -> Result<Vec<Key>, Error> {
        let peer = self
            .peers
            .get_mut(&checkpoint.issuer.node)
            .ok_or(Error::Invalid)?;
        if peer.issuer != checkpoint.issuer
            || peer
                .view
                .as_ref()
                .is_some_and(|view| view.revision >= checkpoint.view.revision)
        {
            return Err(Error::Invalid);
        }
        peer.view = Some(checkpoint.view);
        Ok(checkpoint.retired)
    }

    pub(in crate::control) fn admission_ready(&self, now: Instant) -> bool {
        self.require_fresh_views(now).is_ok()
    }

    pub(in crate::control) fn claim_views_ready(
        &self,
        now: Instant,
        mut current: impl FnMut(NodeId, NonZeroU64, NonZeroU64) -> bool,
    ) -> bool {
        !self.blocked()
            && self.peers.values().all(|peer| {
                peer.view.as_ref().is_some_and(|view| {
                    now < view.deadline
                        && current(peer.issuer.node, peer.issuer.incarnation, view.revision)
                })
            })
    }

    fn retired(&self, key: Key) -> bool {
        self.records.get(&key).is_some_and(|fact| fact.retired)
            || self
                .views()
                .any(|view| view.records.get(&key).is_some_and(|fact| fact.retired))
    }

    fn views(&self) -> impl Iterator<Item = &View> {
        self.peers.values().filter_map(|peer| peer.view.as_ref())
    }

    fn blocked(&self) -> bool {
        self.blocked || self.views().any(|view| view.blocked)
    }

    fn require_fresh_views(&self, now: Instant) -> Result<(), Error> {
        if self.blocked() {
            return Err(Error::NoAuthority);
        }
        if self
            .peers
            .values()
            .any(|peer| peer.view.as_ref().is_none_or(|view| now >= view.deadline))
        {
            return Err(Error::Unfresh);
        }
        Ok(())
    }

    fn remote(&self, issuer: Issuer) -> Option<&Remote> {
        self.peers
            .get(&issuer.node)
            .filter(|peer| peer.issuer == issuer)
    }

    // Issuance and known retirement are monotone within an authority epoch.
    // A covered proof can remain valid after the view expires; admission
    // still requires a fresh view to rule out unseen retirements.
    fn check_coverage(&self, issuer: Issuer, key: Key, expires_at: u64) -> Result<(), Error> {
        if self.retired(key) {
            return Err(Error::Retired);
        }
        if self.blocked() {
            return Err(Error::NoAuthority);
        }
        let recorded = if issuer == self.issuer {
            self.records.get(&key).ok_or(Error::NoAuthority)?
        } else {
            self.remote(issuer)
                .ok_or(Error::NoAuthority)?
                .view
                .as_ref()
                .and_then(|view| view.records.get(&key))
                .ok_or(Error::Unfresh)?
        };
        if recorded.issued_through < expires_at {
            return Err(if issuer != self.issuer {
                Error::Unfresh
            } else {
                Error::NoAuthority
            });
        }
        Ok(())
    }
}

impl PeerTrust for Trust {
    fn issuer(&self) -> Issuer {
        self.issuer
    }

    fn sign(&self, message: &[u8]) -> Option<[u8; SIGNATURE_BYTES]> {
        Some(self.key.sign_message(message))
    }

    fn eligible_key(&self, issuer: Issuer) -> Option<ApiPublicKey> {
        if issuer == self.issuer {
            Some(self.key.public_key())
        } else {
            self.remote(issuer).map(|peer| peer.key)
        }
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
        let key = (endpoint, rank.lineage());
        if self.retired(key) {
            return Err(Error::Retired);
        }
        self.require_fresh_views(Instant::now())?;
        if self.records.len() >= CAPACITY && !self.records.contains_key(&key) {
            return Err(Error::NoAuthority);
        }
        let fact = self.records.entry(key).or_default();
        fact.issued_through = fact.issued_through.max(expires_at);
        Ok(())
    }

    fn check_recovery(
        &self,
        issuer: Issuer,
        endpoint: EndpointKey,
        rank: ClaimRank,
        expires_at: u64,
        _now: u64,
    ) -> Result<(), Error> {
        self.check_coverage(issuer, (endpoint, rank.lineage()), expires_at)
    }

    fn retire(&mut self, endpoint: EndpointKey, rank: ClaimRank) {
        let key = (endpoint, rank.lineage());
        if self.retired(key) || self.blocked {
            return;
        }
        if self.records.len() >= CAPACITY && !self.records.contains_key(&key) {
            // No legal image can encode another fence. Latch rather than
            // granting recovery with an incomplete retirement history.
            self.blocked = true;
            return;
        }
        self.records.entry(key).or_default().retired = true;
    }

    fn is_retired(&self, endpoint: EndpointKey, rank: ClaimRank) -> bool {
        self.retired((endpoint, rank.lineage()))
    }
}

#[cfg(test)]
mod tests;
