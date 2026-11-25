//! Bounded ownership receipts. Trust in an issuer is supplied by the authenticated peer adapter,
//! not by membership observations or participant credentials.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::ClaimRank;
use crate::entity::{ConnectionId, EndpointKey, ParticipantId, ProjectId, ProjectRoomKey, RoomId};
use pulsebeam_core::{
    auth::{ApiPublicKey, ApiSigningKey},
    identity::NodeId,
};
use pulsebeam_proto::state::{MAX_SUCCESSOR_RECEIPT_BYTES, SuccessorReceipt};
use pulsebeam_runtime::rand::RngCore;

const LIFETIME_SECONDS: u64 = 300;
const CLOCK_SKEW_SECONDS: u64 = 5;
const SIGNATURE_BYTES: usize = 64;
const MAX_LINEAGES: usize = 4096;
const SIGNING_DOMAIN: &[u8] = b"pulsebeam.joined.successor.v1\0";

pub(in crate::control) mod epoch;
pub(in crate::control::controller) mod standby;

pub(in crate::control) enum Authority {
    Local(Box<dyn LineageTrust>),
    Provisioned(Box<epoch::Trust>),
}

impl Authority {
    pub(in crate::control) fn ready(&self) -> bool {
        match self {
            Self::Local(_) => true,
            Self::Provisioned(trust) => {
                trust.admission_ready(pulsebeam_runtime::time::Instant::now())
            }
        }
    }
}

impl std::ops::Deref for Authority {
    type Target = dyn LineageTrust;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Local(trust) => trust.as_ref(),
            Self::Provisioned(trust) => trust.as_ref(),
        }
    }
}

impl std::ops::DerefMut for Authority {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Local(trust) => trust.as_mut(),
            Self::Provisioned(trust) => trust.as_mut(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::control) struct Issuer {
    pub trust_domain: [u8; 16],
    pub node: NodeId,
    pub incarnation: std::num::NonZeroU64,
}

/// Implementations use the peer signing key and an authenticated issuer directory.
/// Receipt eligibility deliberately does not imply authority to send a live snapshot.
pub(in crate::control) trait PeerTrust {
    fn issuer(&self) -> Issuer;
    fn sign(&self, message: &[u8]) -> Option<[u8; SIGNATURE_BYTES]>;
    fn eligible_key(&self, issuer: Issuer) -> Option<ApiPublicKey>;
}

/// Verifies lineage proof coverage in addition to issuer eligibility.
/// A validated receipt does not grant admission without `Authority::ready`.
pub(in crate::control) trait LineageTrust: PeerTrust + Send {
    fn record_issuance(
        &mut self,
        endpoint: EndpointKey,
        rank: ClaimRank,
        expires_at: u64,
        now: u64,
    ) -> Result<(), Error>;
    fn check_recovery(
        &self,
        issuer: Issuer,
        endpoint: EndpointKey,
        rank: ClaimRank,
        expires_at: u64,
        now: u64,
    ) -> Result<(), Error>;
    fn retire(&mut self, endpoint: EndpointKey, rank: ClaimRank);
    fn is_retired(&self, endpoint: EndpointKey, rank: ClaimRank) -> bool;
}

pub(in crate::control) fn issue_accepted(
    trust: &mut dyn LineageTrust,
    endpoint: EndpointKey,
    rank: ClaimRank,
    now: u64,
) -> Result<SuccessorReceipt, Error> {
    let receipt = issue(trust, endpoint, rank, now)?;
    trust.record_issuance(endpoint, rank, receipt.expires_at_unix_seconds, now)?;
    Ok(receipt)
}

pub(in crate::control) fn validate_successor(
    trust: &dyn LineageTrust,
    proof: &[u8],
    endpoint: EndpointKey,
    connection: ConnectionId,
    now: u64,
) -> Result<ClaimRank, Error> {
    successor(
        trust,
        proof,
        endpoint,
        connection,
        now,
        |issuer, endpoint, rank, expires_at| {
            trust.check_recovery(issuer, endpoint, rank, expires_at, now)
        },
    )
}

/// A standalone process has only its own epoch in the trust directory. A
/// restart cannot validate receipts from the lost epoch.
pub(in crate::control) struct StandaloneTrust {
    issuer: Issuer,
    key: ApiSigningKey,
    lineages: BTreeMap<(EndpointKey, ConnectionId), Lineage>,
}

struct Lineage {
    expires_at: u64,
    retired: bool,
}

impl StandaloneTrust {
    pub(in crate::control) fn new(entropy: &mut (impl RngCore + rand::CryptoRng)) -> Self {
        let mut trust_domain = [0; 16];
        let mut seed = [0; 32];
        entropy.fill_bytes(&mut trust_domain);
        entropy.fill_bytes(&mut seed);
        Self {
            issuer: Issuer {
                trust_domain,
                node: NodeId::new(),
                incarnation: std::num::NonZeroU64::MIN,
            },
            key: ApiSigningKey::from_seed(seed),
            lineages: BTreeMap::new(),
        }
    }
}

impl LineageTrust for StandaloneTrust {
    fn record_issuance(
        &mut self,
        endpoint: EndpointKey,
        rank: ClaimRank,
        expires_at: u64,
        now: u64,
    ) -> Result<(), Error> {
        let key = (endpoint, rank.lineage());
        if !self.lineages.contains_key(&key) && self.lineages.len() == MAX_LINEAGES {
            self.lineages
                .retain(|_, lineage| eligible_through(lineage.expires_at) >= now);
            if self.lineages.len() == MAX_LINEAGES {
                return Err(Error::NoAuthority);
            }
        }
        if self
            .lineages
            .get(&key)
            .is_some_and(|lineage| lineage.retired)
        {
            return Err(Error::Retired);
        }
        self.lineages
            .entry(key)
            .and_modify(|lineage| lineage.expires_at = lineage.expires_at.max(expires_at))
            .or_insert(Lineage {
                expires_at,
                retired: false,
            });
        Ok(())
    }

    fn retire(&mut self, endpoint: EndpointKey, rank: ClaimRank) {
        if let Some(lineage) = self.lineages.get_mut(&(endpoint, rank.lineage())) {
            lineage.retired = true;
        }
    }

    fn is_retired(&self, endpoint: EndpointKey, rank: ClaimRank) -> bool {
        self.lineages
            .get(&(endpoint, rank.lineage()))
            .is_some_and(|lineage| lineage.retired)
    }

    fn check_recovery(
        &self,
        issuer: Issuer,
        endpoint: EndpointKey,
        rank: ClaimRank,
        expires_at: u64,
        now: u64,
    ) -> Result<(), Error> {
        if issuer != self.issuer {
            return Err(Error::NoAuthority);
        }
        let lineage = self
            .lineages
            .get(&(endpoint, rank.lineage()))
            .ok_or(Error::NoAuthority)?;
        if lineage.retired {
            return Err(Error::Retired);
        }
        if expires_at > lineage.expires_at {
            return Err(Error::NoAuthority);
        }
        if now > eligible_through(lineage.expires_at) {
            return Err(Error::Expired);
        }
        Ok(())
    }
}

fn eligible_through(expires_at: u64) -> u64 {
    expires_at.saturating_add(CLOCK_SKEW_SECONDS)
}

impl PeerTrust for StandaloneTrust {
    fn issuer(&self) -> Issuer {
        self.issuer
    }

    fn sign(&self, message: &[u8]) -> Option<[u8; SIGNATURE_BYTES]> {
        Some(self.key.sign_message(message))
    }

    fn eligible_key(&self, issuer: Issuer) -> Option<ApiPublicKey> {
        (self.issuer == issuer).then(|| self.key.public_key())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claims {
    issuer: Issuer,
    project: ProjectId,
    room: RoomId,
    participant: ParticipantId,
    root: ConnectionId,
    position: u64,
    predecessor: ConnectionId,
    expires_at: u64,
}

impl Claims {
    fn endpoint(&self) -> EndpointKey {
        EndpointKey {
            room: ProjectRoomKey {
                project: self.project,
                room: self.room,
            },
            participant: self.participant,
        }
    }

    fn rank(&self) -> ClaimRank {
        ClaimRank {
            root: self.root,
            position: self.position,
            connection: self.predecessor,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) enum Error {
    Invalid,
    Scope,
    Expired,
    Retired,
    Unfresh,
    NoAuthority,
}

/// Call only for the accepted live joined claim. An adapter outage is non-issuing.
fn issue(
    trust: &(impl PeerTrust + ?Sized),
    endpoint: EndpointKey,
    rank: ClaimRank,
    now: u64,
) -> Result<SuccessorReceipt, Error> {
    let claims = Claims {
        issuer: trust.issuer(),
        project: endpoint.room.project,
        room: endpoint.room.room,
        participant: endpoint.participant,
        root: rank.root,
        position: rank.position,
        predecessor: rank.connection,
        expires_at: now.checked_add(LIFETIME_SECONDS).ok_or(Error::Invalid)?,
    };
    let mut receipt = SIGNING_DOMAIN.to_vec();
    serde_json::to_writer(&mut receipt, &claims).map_err(|_| Error::Invalid)?;
    if receipt.len() > MAX_SUCCESSOR_RECEIPT_BYTES - SIGNATURE_BYTES {
        return Err(Error::Invalid);
    }
    let signature = trust.sign(&receipt).ok_or(Error::NoAuthority)?;
    receipt.extend_from_slice(&signature);
    Ok(SuccessorReceipt {
        proof: receipt,
        expires_at_unix_seconds: claims.expires_at,
    })
}

fn successor(
    trust: &(impl PeerTrust + ?Sized),
    receipt: &[u8],
    endpoint: EndpointKey,
    connection: ConnectionId,
    now: u64,
    check_retirement: impl FnOnce(Issuer, EndpointKey, ClaimRank, u64) -> Result<(), Error>,
) -> Result<ClaimRank, Error> {
    if receipt.len() > MAX_SUCCESSOR_RECEIPT_BYTES {
        return Err(Error::Invalid);
    }
    let (message, signature) = receipt
        .split_last_chunk::<SIGNATURE_BYTES>()
        .ok_or(Error::Invalid)?;
    let payload = message.strip_prefix(SIGNING_DOMAIN).ok_or(Error::Invalid)?;
    let claims: Claims = serde_json::from_slice(payload).map_err(|_| Error::Invalid)?;
    if claims.endpoint() != endpoint {
        return Err(Error::Scope);
    }
    if claims.issuer.trust_domain != trust.issuer().trust_domain {
        // The domain is untrusted here. It can rule out verification in this
        // epoch, but it cannot establish the receipt's authenticity.
        return Err(Error::NoAuthority);
    }
    if !trust
        .eligible_key(claims.issuer)
        .is_some_and(|key| key.verify_message(message, signature))
    {
        return Err(Error::Invalid);
    }
    if now > eligible_through(claims.expires_at)
        || claims.expires_at > now.saturating_add(LIFETIME_SECONDS + CLOCK_SKEW_SECONDS)
    {
        return Err(Error::Expired);
    }
    let rank = claims.rank();
    check_retirement(claims.issuer, endpoint, rank, claims.expires_at)?;
    rank.successor(connection).ok_or(Error::Invalid)
}

#[cfg(test)]
pub(in crate::control::controller) mod tests {
    use std::num::NonZeroU64;

    use super::*;

    struct TrustFixture {
        local: Issuer,
        eligible_issuer: Issuer,
        eligible_key: ApiPublicKey,
        local_key: Option<ApiSigningKey>,
        recovery: Option<RecoveryFact>,
    }

    #[derive(Clone, Copy)]
    struct RecoveryFact {
        endpoint: EndpointKey,
        lineage: ConnectionId,
        fresh_through: u64,
        retired: bool,
    }

    impl LineageTrust for TrustFixture {
        fn record_issuance(
            &mut self,
            _: EndpointKey,
            _: ClaimRank,
            _: u64,
            _: u64,
        ) -> Result<(), Error> {
            Err(Error::NoAuthority)
        }

        fn check_recovery(
            &self,
            issuer: Issuer,
            endpoint: EndpointKey,
            rank: ClaimRank,
            _expires_at: u64,
            now: u64,
        ) -> Result<(), Error> {
            let fact = self
                .recovery
                .filter(|fact| {
                    issuer == self.eligible_issuer
                        && fact.endpoint == endpoint
                        && fact.lineage == rank.lineage()
                })
                .ok_or(Error::NoAuthority)?;
            if fact.retired {
                return Err(Error::Retired);
            }
            if now > fact.fresh_through {
                return Err(Error::NoAuthority);
            }
            Ok(())
        }

        fn retire(&mut self, endpoint: EndpointKey, rank: ClaimRank) {
            if let Some(fact) = self
                .recovery
                .as_mut()
                .filter(|fact| fact.endpoint == endpoint && fact.lineage == rank.lineage())
            {
                fact.retired = true;
            }
        }

        fn is_retired(&self, endpoint: EndpointKey, rank: ClaimRank) -> bool {
            self.recovery.is_some_and(|fact| {
                fact.endpoint == endpoint && fact.lineage == rank.lineage() && fact.retired
            })
        }
    }

    impl PeerTrust for TrustFixture {
        fn issuer(&self) -> Issuer {
            self.local
        }

        fn sign(&self, message: &[u8]) -> Option<[u8; SIGNATURE_BYTES]> {
            Some(self.local_key.as_ref()?.sign_message(message))
        }

        fn eligible_key(&self, issuer: Issuer) -> Option<ApiPublicKey> {
            (issuer == self.eligible_issuer).then_some(self.eligible_key)
        }
    }

    pub(in crate::control::controller) fn offline_issuer_fixture(
        endpoint: EndpointKey,
        predecessor: ClaimRank,
        now: u64,
        fresh_through: u64,
    ) -> (SuccessorReceipt, Box<dyn LineageTrust>) {
        let issuer = Issuer {
            trust_domain: [11; 16],
            node: NodeId::new(),
            incarnation: NonZeroU64::MIN,
        };
        let key = ApiSigningKey::from_seed([21; 32]);
        let primary = TrustFixture {
            local: issuer,
            eligible_issuer: issuer,
            eligible_key: key.public_key(),
            local_key: Some(key),
            recovery: None,
        };
        let receipt = issue(&primary, endpoint, predecessor, now).unwrap();
        let receiving_trust = TrustFixture {
            local: Issuer {
                node: NodeId::new(),
                ..issuer
            },
            local_key: None,
            recovery: Some(RecoveryFact {
                endpoint,
                lineage: predecessor.lineage(),
                fresh_through,
                retired: false,
            }),
            ..primary
        };
        (receipt, Box::new(receiving_trust))
    }

    #[test]
    fn standalone_signature_cannot_restore_a_missing_or_retired_lineage() {
        let mut entropy = pulsebeam_runtime::rand::seeded_rng(91);
        let mut trust = StandaloneTrust::new(&mut entropy);
        let endpoint = EndpointKey {
            room: ProjectRoomKey::fixture(RoomId::new("stage").unwrap()),
            participant: ParticipantId::new("alice").unwrap(),
        };
        let predecessor = ClaimRank::root(ConnectionId::new());
        let next = ConnectionId::new();
        let unrecorded = issue(&trust, endpoint, predecessor, 1000).unwrap();
        assert_eq!(
            validate_successor(&trust, &unrecorded.proof, endpoint, next, 1000),
            Err(Error::NoAuthority)
        );
        let recorded = issue_accepted(&mut trust, endpoint, predecessor, 1000).unwrap();
        assert_eq!(
            validate_successor(&trust, &recorded.proof, endpoint, next, 1000),
            Ok(predecessor.successor(next).unwrap())
        );
        let restarted = StandaloneTrust::new(&mut entropy);
        assert_eq!(
            validate_successor(&restarted, &recorded.proof, endpoint, next, 1000),
            Err(Error::NoAuthority)
        );
        trust.retire(endpoint, predecessor);
        assert_eq!(
            validate_successor(&trust, &recorded.proof, endpoint, next, 1000),
            Err(Error::Retired)
        );
        assert_eq!(
            issue_accepted(&mut trust, endpoint, predecessor, 1000),
            Err(Error::Retired)
        );
    }

    #[test]
    fn validates_issuer_scope_expiry_and_retirement_before_issuing_successor_rank() {
        let issuer = Issuer {
            trust_domain: [8; 16],
            node: NodeId::new(),
            incarnation: NonZeroU64::MIN,
        };
        let signing_key = ApiSigningKey::from_seed([7; 32]);
        let primary = TrustFixture {
            local: issuer,
            eligible_issuer: issuer,
            eligible_key: signing_key.public_key(),
            local_key: Some(signing_key),
            recovery: None,
        };
        let remote = TrustFixture {
            local: Issuer {
                node: NodeId::new(),
                ..issuer
            },
            local_key: None,
            ..primary
        };
        let endpoint = EndpointKey {
            room: ProjectRoomKey::fixture(RoomId::new("stage").unwrap()),
            participant: ParticipantId::new("alice").unwrap(),
        };
        let predecessor = ClaimRank::root(ConnectionId::new());
        let candidate = ConnectionId::new();
        let issued = issue(&primary, endpoint, predecessor, 1000).unwrap();
        assert_eq!(issued.expires_at_unix_seconds, 1300);
        let receipt = issued.proof;
        let check = |bytes: &[u8], scope, now| {
            successor(
                &remote,
                bytes,
                scope,
                candidate,
                now,
                |verified_issuer, key, rank, expires_at| {
                    assert_eq!(expires_at, 1300);
                    assert!(verified_issuer == issuer);
                    assert_eq!(key, endpoint);
                    assert_eq!(rank, predecessor);
                    Ok(())
                },
            )
        };
        assert_eq!(
            check(&receipt, endpoint, 1000),
            Ok(predecessor.successor(candidate).unwrap())
        );
        assert_eq!(check(&receipt, endpoint, 1306), Err(Error::Expired));
        assert_eq!(
            validate_successor(&remote, &receipt, endpoint, candidate, 1000),
            Err(Error::NoAuthority)
        );
        let mut synchronized = TrustFixture {
            local_key: None,
            recovery: Some(RecoveryFact {
                endpoint,
                lineage: predecessor.lineage(),
                fresh_through: 1020,
                retired: false,
            }),
            ..remote
        };
        assert_eq!(
            validate_successor(&synchronized, &receipt, endpoint, candidate, 1020),
            Ok(predecessor.successor(candidate).unwrap())
        );
        assert_eq!(
            validate_successor(&synchronized, &receipt, endpoint, candidate, 1021),
            Err(Error::NoAuthority)
        );
        synchronized.retire(endpoint, predecessor);
        assert_eq!(
            validate_successor(&synchronized, &receipt, endpoint, candidate, 1020),
            Err(Error::Retired)
        );
        let other = EndpointKey {
            participant: ParticipantId::new("bob").unwrap(),
            ..endpoint
        };
        assert_eq!(check(&receipt, other, 1000), Err(Error::Scope));
        assert_eq!(
            successor(
                &remote,
                &receipt,
                endpoint,
                candidate,
                1000,
                |_, _, _, _| Err(Error::Retired)
            ),
            Err(Error::Retired)
        );
        let mut corrupt = receipt.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert_eq!(check(&corrupt, endpoint, 1000), Err(Error::Invalid));
        let wrong_key = TrustFixture {
            eligible_key: ApiSigningKey::from_seed([9; 32]).public_key(),
            local_key: None,
            ..remote
        };
        assert_eq!(
            successor(
                &wrong_key,
                &receipt,
                endpoint,
                candidate,
                1000,
                |_, _, _, _| Ok(())
            ),
            Err(Error::Invalid)
        );
        let ineligible = TrustFixture {
            eligible_issuer: Issuer {
                incarnation: NonZeroU64::new(2).unwrap(),
                ..issuer
            },
            ..remote
        };
        assert_eq!(
            successor(
                &ineligible,
                &receipt,
                endpoint,
                candidate,
                1000,
                |_, _, _, _| Ok(())
            ),
            Err(Error::Invalid)
        );
    }
}
