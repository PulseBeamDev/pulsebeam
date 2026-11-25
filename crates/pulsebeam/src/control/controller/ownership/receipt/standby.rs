use super::{Error, Issuer, LineageTrust, SIGNATURE_BYTES};
use crate::control::controller::ownership::{ClaimRank, observations::PublicationFact};
use crate::entity::{ConnectionId, EndpointKey, PublicationKey, TrackId};
use pulsebeam_core::identity::NodeId;
use std::num::NonZeroU64;

pub(in crate::control::controller) mod wire;

const DOMAIN: &[u8] = b"pulsebeam.publisher.standby.v1\0";
const MAX_LIFETIME_SECONDS: u64 = 30;
const MAX_STANDBYS: usize = 4;

/// Obtained from current participant ownership, never from the candidate's
/// grant, advertised media, or availability of a network route.
#[derive(Clone, Copy)]
pub(in crate::control::controller) struct Owner {
    pub(in crate::control::controller) endpoint: EndpointKey,
    pub(in crate::control::controller) issuer: Issuer,
    pub(in crate::control::controller) rank: ClaimRank,
    pub(in crate::control::controller) authorization: u64,
    pub(in crate::control::controller) expires_at: u64,
}

/// A logical label and one exact encoder incarnation. The receiver supplies
/// these facts from the authenticated origin and the proposed publication.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::control::controller) struct Source {
    pub(in crate::control::controller) node: NodeId,
    pub(in crate::control::controller) generation: NonZeroU64,
    pub(in crate::control::controller) connection: ConnectionId,
    pub(in crate::control::controller) publication: PublicationKey,
    pub(in crate::control::controller) track: TrackId,
}

impl Source {
    pub(in crate::control::controller) fn materialize(
        self,
        endpoint: EndpointKey,
        origin: (NodeId, NonZeroU64),
        connection: ConnectionId,
        publication: &PublicationFact,
    ) -> Option<crate::track::Track> {
        if (self.node, self.generation) != origin || self.connection != connection {
            return None;
        }
        let track = publication.media_track(endpoint, connection)?;
        (track.id() == self.publication && track.meta().track_id == Some(self.track))
            .then_some(track)
    }
}

#[derive(Clone, Copy)]
pub(in crate::control::controller) struct Grant {
    expires_at: u64,
    signature: [u8; SIGNATURE_BYTES],
}

impl Owner {
    pub(in crate::control::controller) fn same_epoch(self, other: Self) -> bool {
        self.endpoint == other.endpoint
            && self.issuer == other.issuer
            && self.rank == other.rank
            && self.authorization == other.authorization
    }

    fn message(self, source: Source, expires_at: u64) -> Result<Vec<u8>, Error> {
        let mut message = Vec::with_capacity(256);
        message.extend_from_slice(DOMAIN);
        message.extend_from_slice(&self.issuer.trust_domain);
        message.extend_from_slice(self.issuer.node.as_bytes());
        message.extend_from_slice(&self.issuer.incarnation.get().to_be_bytes());
        message.extend_from_slice(self.endpoint.room.project.as_bytes());
        for name in [
            self.endpoint.room.room.as_str(),
            self.endpoint.participant.as_str(),
            source.track.as_str(),
        ] {
            let len = u16::try_from(name.len()).map_err(|_| Error::Invalid)?;
            message.extend_from_slice(&len.to_be_bytes());
            message.extend_from_slice(name.as_bytes());
        }
        message.extend_from_slice(self.rank.root.as_bytes());
        message.extend_from_slice(&self.rank.position.to_be_bytes());
        message.extend_from_slice(self.rank.connection.as_bytes());
        message.extend_from_slice(&self.authorization.to_be_bytes());
        message.extend_from_slice(source.node.as_bytes());
        message.extend_from_slice(&source.generation.get().to_be_bytes());
        message.extend_from_slice(source.connection.as_bytes());
        message.push(source.publication.kind() as u8);
        message.extend_from_slice(source.publication.as_bytes());
        message.extend_from_slice(&expires_at.to_be_bytes());
        Ok(message)
    }

    fn check_lifetime(
        self,
        trust: &dyn LineageTrust,
        expires_at: u64,
        now: u64,
    ) -> Result<(), Error> {
        if trust.is_retired(self.endpoint, self.rank) {
            return Err(Error::Retired);
        }
        if expires_at <= now
            || expires_at > self.expires_at
            || expires_at > now.saturating_add(MAX_LIFETIME_SECONDS)
        {
            return Err(Error::Expired);
        }
        Ok(())
    }

    pub(in crate::control::controller) fn issue(
        self,
        trust: &dyn LineageTrust,
        source: Source,
        expires_at: u64,
        now: u64,
    ) -> Result<Grant, Error> {
        if self.issuer != trust.issuer() {
            return Err(Error::NoAuthority);
        }
        self.check_lifetime(trust, expires_at, now)?;
        let signature = trust
            .sign(&self.message(source, expires_at)?)
            .ok_or(Error::NoAuthority)?;
        Ok(Grant {
            expires_at,
            signature,
        })
    }

    pub(in crate::control::controller) fn validate(
        self,
        trust: &dyn LineageTrust,
        source: Source,
        grant: Grant,
        now: u64,
    ) -> Result<u64, Error> {
        self.check_lifetime(trust, grant.expires_at, now)?;
        let key = trust.eligible_key(self.issuer).ok_or(Error::NoAuthority)?;
        key.verify_message(&self.message(source, grant.expires_at)?, &grant.signature)
            .then_some(grant.expires_at)
            .ok_or(Error::Invalid)
    }
}

/// One logical publication for one live owner epoch. Admission changes the
/// candidate set, not what is selected, and never takes a network address.
pub(in crate::control::controller) struct Candidates {
    owner: Owner,
    primary: Source,
    standbys: Vec<(Source, Grant)>,
    selected: Option<Source>,
    selection_revision: u64,
}

impl Candidates {
    pub(in crate::control::controller) fn new(
        owner: Owner,
        primary: Source,
    ) -> Result<Self, Error> {
        if primary.node != owner.issuer.node
            || primary.generation != owner.issuer.incarnation
            || primary.connection != owner.rank.connection()
        {
            return Err(Error::Scope);
        }
        Ok(Self {
            owner,
            primary,
            standbys: Vec::new(),
            selected: Some(primary),
            selection_revision: 0,
        })
    }

    fn check_owner(&self, current: Owner, trust: &dyn LineageTrust, now: u64) -> Result<(), Error> {
        if !self.owner.same_epoch(current) {
            return Err(Error::Scope);
        }
        if trust.eligible_key(current.issuer).is_none() {
            return Err(Error::NoAuthority);
        }
        if current.expires_at <= now {
            return Err(Error::Expired);
        }
        if trust.is_retired(current.endpoint, current.rank) {
            return Err(Error::Retired);
        }
        Ok(())
    }

    pub(in crate::control::controller) fn admit(
        &mut self,
        current: Owner,
        trust: &dyn LineageTrust,
        source: Source,
        grant: Grant,
        now: u64,
    ) -> Result<(), Error> {
        self.check_owner(current, trust, now)?;
        if source.track != self.primary.track
            || source.publication.kind() != self.primary.publication.kind()
            || source.publication == self.primary.publication
        {
            return Err(Error::Scope);
        }
        current.validate(trust, source, grant, now)?;
        if self
            .selected
            .is_some_and(|selected| self.check_source(current, trust, selected, now).is_err())
        {
            self.selected = None;
        }
        self.standbys.retain(|(_, grant)| grant.expires_at > now);
        if let Some((existing, existing_grant)) = self
            .standbys
            .iter_mut()
            .find(|(existing, _)| existing.publication == source.publication)
        {
            if *existing != source {
                return Err(Error::Scope);
            }
            // Never shorten a still-valid delegated lease due to reordering.
            if grant.expires_at > existing_grant.expires_at {
                *existing_grant = grant;
            }
        } else {
            if self.standbys.len() >= MAX_STANDBYS {
                return Err(Error::NoAuthority);
            }
            self.standbys.push((source, grant));
        }
        Ok(())
    }

    pub(in crate::control::controller) fn select(
        &mut self,
        current: Owner,
        trust: &dyn LineageTrust,
        revision: NonZeroU64,
        source: Source,
        now: u64,
    ) -> Result<(), Error> {
        self.check_owner(current, trust, now)?;
        let revision = revision.get();
        if revision < self.selection_revision
            || (revision == self.selection_revision && self.selected != Some(source))
        {
            return Err(Error::Invalid);
        }
        self.check_source(current, trust, source, now)?;
        self.selected = Some(source);
        self.selection_revision = revision;
        Ok(())
    }

    pub(in crate::control::controller) fn selected(
        &self,
        current: Owner,
        trust: &dyn LineageTrust,
        now: u64,
    ) -> Option<Source> {
        self.check_owner(current, trust, now).ok()?;
        let selected = self.selected?;
        self.check_source(current, trust, selected, now).ok()?;
        Some(selected)
    }

    pub(in crate::control::controller) fn selection(
        &self,
        owner: Owner,
        trust: &dyn LineageTrust,
        now: u64,
    ) -> Option<(u64, Source, u64)> {
        let selected = self.selected(owner, trust, now)?;
        let deadline = self
            .standbys
            .iter()
            .find(|(source, _)| *source == selected)
            .map_or(owner.expires_at, |(_, grant)| grant.expires_at);
        Some((self.selection_revision, selected, deadline))
    }

    pub(in crate::control::controller) fn primary(&self) -> Source {
        self.primary
    }

    fn check_source(
        &self,
        current: Owner,
        trust: &dyn LineageTrust,
        source: Source,
        now: u64,
    ) -> Result<(), Error> {
        if source == self.primary {
            return Ok(());
        }
        let (_, grant) = self
            .standbys
            .iter()
            .find(|(candidate, _)| *candidate == source)
            .ok_or(Error::NoAuthority)?;
        current.check_lifetime(trust, grant.expires_at, now)
    }
}

#[cfg(test)]
mod tests;
