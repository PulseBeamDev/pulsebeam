//! Receipt authority for one fixed pair and one provisioned authority epoch.
//! Neither an expired view nor a Link reconnect forgets terminal lineage facts.

use std::{collections::BTreeMap, num::NonZeroU64};

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
    view: View,
    retired: Vec<Key>,
}

impl Validated {
    pub(in crate::control) fn blocked(&self) -> bool {
        self.view.blocked
    }
}

pub(in crate::control) struct Trust {
    issuer: Issuer,
    remote: Issuer,
    key: ApiSigningKey,
    remote_key: ApiPublicKey,
    blocked: bool,
    records: Ledger,
    view: Option<View>,
}

impl Trust {
    pub(in crate::control) fn new(
        issuer: Issuer,
        key: ApiSigningKey,
        remote: Issuer,
        remote_key: ApiPublicKey,
    ) -> Result<Self, Error> {
        if issuer.trust_domain != remote.trust_domain
            || issuer.node == remote.node
            || key.public_key() == remote_key
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            issuer,
            remote,
            key,
            remote_key,
            blocked: false,
            records: Ledger::new(),
            view: None,
        })
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
        revision: NonZeroU64,
        checkpoint: Checkpoint,
        now: Instant,
        deadline: Instant,
    ) -> Result<Validated, Error> {
        if now >= deadline {
            return Err(Error::Unfresh);
        }
        if checkpoint.issuer != self.remote || checkpoint.records.len() > CAPACITY {
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
        if let Some(old) = &self.view {
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
            view: View {
                revision,
                deadline,
                blocked: checkpoint.blocked,
                records,
            },
            retired: newly_retired,
        })
    }

    pub(in crate::control) fn install_checkpoint(&mut self, checkpoint: Validated) -> Vec<Key> {
        self.view = Some(checkpoint.view);
        checkpoint.retired
    }

    pub(in crate::control) fn admission_ready(&self, now: Instant) -> bool {
        self.fresh_view(now).is_ok()
    }

    fn retired(&self, key: Key) -> bool {
        self.records.get(&key).is_some_and(|fact| fact.retired)
            || self
                .view
                .as_ref()
                .and_then(|view| view.records.get(&key))
                .is_some_and(|fact| fact.retired)
    }

    fn fresh_view(&self, now: Instant) -> Result<&View, Error> {
        if self.blocked || self.view.as_ref().is_some_and(|view| view.blocked) {
            return Err(Error::NoAuthority);
        }
        self.view
            .as_ref()
            .filter(|view| now < view.deadline)
            .ok_or(Error::Unfresh)
    }

    fn recover_at(
        &self,
        issuer: Issuer,
        key: Key,
        expires_at: u64,
        now: Instant,
    ) -> Result<(), Error> {
        if self.retired(key) {
            return Err(Error::Retired);
        }
        let view = self.fresh_view(now)?;
        let recorded = if issuer == self.issuer {
            self.records.get(&key).ok_or(Error::NoAuthority)?
        } else if issuer == self.remote {
            view.records.get(&key).ok_or(Error::Unfresh)?
        } else {
            return Err(Error::NoAuthority);
        };
        if recorded.issued_through < expires_at {
            return Err(if issuer == self.remote {
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
            (issuer == self.remote).then_some(self.remote_key)
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
        self.fresh_view(Instant::now())?;
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
        self.recover_at(
            issuer,
            (endpoint, rank.lineage()),
            expires_at,
            Instant::now(),
        )
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
