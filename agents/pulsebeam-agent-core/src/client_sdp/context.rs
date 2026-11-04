use super::identity_history::IdentityHistory;
use super::sdp::{self, Document, Transport};
use super::{
    Allocation, Binding, Bindings, Capabilities, Codec, Encoding, Error, Kind, Limits, MediaOwner,
    Preparation, RtcpMode, RtcpParameters, RuntimeEncoding,
};
use std::collections::{BTreeMap, BTreeSet};

mod receiver;
mod sender;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    /// Failed or cancelled before the PeerConnection applied the answer.
    NotApplied,
    Applied,
    /// Applied SDP remains history, but the host must not declare its media.
    AppliedButCancelled,
}

#[derive(Clone)]
pub struct Candidate {
    id: CandidateId,
    answer: String,
    bindings: Bindings,
}

impl std::fmt::Debug for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Candidate")
            .field("id", &self.id)
            .field("answer_bytes", &self.answer.len())
            .field("bindings", &self.bindings)
            .finish()
    }
}

impl Candidate {
    pub const fn id(&self) -> CandidateId {
        self.id
    }
    pub fn answer(&self) -> &str {
        &self.answer
    }
    pub const fn bindings(&self) -> &Bindings {
        &self.bindings
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReceptionPlan {
    Bootstrap,
    Authoritative,
}

#[derive(Clone)]
struct Accepted {
    document: Document,
    bindings: Bindings,
    retained: Bindings,
    local: Document,
    declarable: bool,
    reception_plans: BTreeMap<String, ReceptionPlan>,
}

impl Accepted {
    fn unclaimed_reception(&self, mid: &str) -> bool {
        self.reception_plans.get(mid) == Some(&ReceptionPlan::Bootstrap)
    }
}

/// One context per PeerConnection. No ambient entropy, network or async state.
pub struct Context {
    local_transport: Transport,
    remote_transport: Transport,
    capabilities: Capabilities,
    accepted: Accepted,
    pending: Option<(CandidateId, Accepted)>,
    identities: IdentityHistory,
    next_identity: u32,
    next_candidate: u64,
}

impl Context {
    pub fn new(
        bootstrap_offer: &str,
        applied_answer: &str,
        capabilities: Capabilities,
        seed: u32,
        limits: Limits,
    ) -> Result<Self, Error> {
        validate_capabilities(&capabilities)?;
        let local = Document::parse(bootstrap_offer)?;
        let remote = Document::parse(applied_answer)?;
        if local.media.len() != remote.media.len()
            || local
                .media
                .iter()
                .zip(&remote.media)
                .any(|(a, b)| a.section.mid != b.section.mid || a.section.kind != b.section.kind)
        {
            return Err(Error::Malformed("bootstrap alignment"));
        }
        for (offer, answer) in local.media.iter().zip(&remote.media) {
            if answer.section.rtcp.reduced_size && !offer.section.rtcp.reduced_size {
                return Err(Error::Envelope(answer.section.mid.clone()));
            }
        }
        let local_transport = active_transport(&local)?;
        let remote_transport = active_transport(&remote)?;
        let mut sender_identities = BTreeMap::new();
        for media in &local.media {
            for (primary, repair) in &media.signaled {
                sender_identities.insert(*primary, (media.section.mid.clone(), None, false));
                if let Some(repair) = repair {
                    sender_identities.insert(*repair, (media.section.mid.clone(), None, true));
                }
            }
        }
        let mut used = BTreeSet::new();
        for doc in [&local, &remote] {
            for media in &doc.media {
                for (primary, repair) in &media.signaled {
                    for id in [Some(*primary), *repair].into_iter().flatten() {
                        if !used.insert(id) {
                            return Err(Error::Collision);
                        }
                    }
                }
            }
        }
        let identities = IdentityHistory::new(used, sender_identities, limits.max_identity_bytes)?;
        let reception_plans = remote
            .media
            .iter()
            .filter(|media| {
                !media.section.stopped
                    && media.section.kind != Kind::Application
                    && media.section.direction.sends()
            })
            .map(|media| (media.section.mid.clone(), ReceptionPlan::Bootstrap))
            .collect();
        Ok(Self {
            local_transport,
            remote_transport,
            capabilities,
            accepted: Accepted {
                document: remote,
                bindings: Bindings::default(),
                retained: Bindings::default(),
                local,
                declarable: false,
                reception_plans,
            },
            pending: None,
            identities,
            next_identity: seed,
            next_candidate: 0,
        })
    }

    pub(crate) fn bootstrap_payload_type(&self, mid: &str) -> Result<Option<u8>, Error> {
        let media = self
            .accepted
            .document
            .media
            .iter()
            .find(|media| media.section.mid == mid)
            .ok_or(Error::Identifier)?;
        if media.section.stopped
            || media.section.kind == Kind::Application
            || media.section.direction == super::Direction::Inactive
        {
            return Ok(None);
        }
        media
            .section
            .codecs
            .iter()
            .find_map(|codec| self.select_codec(codec).ok())
            .map(|codec| Some(codec.payload_type))
            .ok_or_else(|| Error::Envelope(mid.into()))
    }

    /// Correlate media already applied during bootstrap without applying another
    /// answer. Call once before preparing subsequent candidates. SCTP-only
    /// bootstrap needs no adoption. Keys remain caller-owned even for bootstrap.
    pub fn adopt_bootstrap_media(&mut self, requests: &[Preparation]) -> Result<&Bindings, Error> {
        if self.next_candidate != 0 || self.pending.is_some() || self.accepted.declarable {
            return Err(Error::CandidateState);
        }
        if requests.len() > self.accepted.document.media.len() {
            return Err(Error::Capacity);
        }
        let mut requested = BTreeMap::new();
        for request in requests {
            if requested.insert(request.mid.as_str(), request).is_some() {
                return Err(Error::Identifier);
            }
        }
        let mut bindings = Bindings::default();
        for remote in &self.accepted.document.media {
            if remote.section.kind == Kind::Application || remote.section.stopped {
                continue;
            }
            let mid = &remote.section.mid;
            let request = requested.remove(mid.as_str());
            if remote.section.direction == super::Direction::Inactive && request.is_none() {
                continue;
            }
            let request = request.ok_or_else(|| Error::MissingConfiguration(mid.clone()))?;
            self.check_rtcp_mode(remote.section.rtcp.reduced_size)?;
            let local = self
                .accepted
                .local
                .media
                .iter()
                .find(|m| m.section.mid == *mid)
                .ok_or(Error::Identifier)?;
            let codec = remote
                .section
                .codecs
                .iter()
                .find(|c| c.payload_type == request.payload_type)
                .ok_or_else(|| Error::Envelope(mid.clone()))?;
            let codec = self.select_codec(codec)?;
            let offered = local
                .section
                .codecs
                .iter()
                .find(|c| c.payload_type == codec.payload_type)
                .ok_or_else(|| Error::Envelope(mid.clone()))?;
            if offered.parameters.name != codec.parameters.name
                || offered.parameters.clock_rate != codec.parameters.clock_rate
                || offered.parameters.channels != codec.parameters.channels
                || offered.parameters.fmtp != codec.parameters.fmtp
                || codec.repair.is_some() && codec.repair != offered.repair
            {
                return Err(Error::Envelope(mid.clone()));
            }
            if remote.section.direction.receives() && !local.section.direction.sends()
                || remote.section.direction.sends() && !local.section.direction.receives()
            {
                return Err(Error::Envelope(mid.clone()));
            }
            for (publication, sending, owner, encodings, target) in [
                (
                    true,
                    remote.section.direction.receives(),
                    &request.publication,
                    &local.section.encodings,
                    &mut bindings.publications,
                ),
                (
                    false,
                    remote.section.direction.sends(),
                    &request.reception,
                    &remote.section.encodings,
                    &mut bindings.receptions,
                ),
            ] {
                if !sending {
                    if owner.is_some() {
                        return Err(Error::Envelope(mid.clone()));
                    }
                    continue;
                }
                let owner = owner
                    .as_ref()
                    .ok_or_else(|| Error::MissingConfiguration(mid.clone()))?;
                validate_owner(owner)?;
                let mut encodings = encodings.clone();
                if codec.repair.is_none() {
                    for e in &mut encodings {
                        e.repair = None;
                    }
                }
                if encodings.is_empty() {
                    encodings.push(Encoding {
                        rid: None,
                        primary: None,
                        repair: None,
                        paused: false,
                        runtime: RuntimeEncoding::default(),
                    });
                }
                let extensions = remote
                    .section
                    .extensions
                    .iter()
                    .filter(|e| {
                        self.capabilities.extensions.contains(&e.uri)
                            && if publication {
                                e.direction.receives()
                            } else {
                                e.direction.sends()
                            }
                    })
                    .map(|e| {
                        let mut e = e.clone();
                        e.direction = e.direction.inverse();
                        e
                    })
                    .collect();
                target.push(Binding {
                    key: owner.key.clone(),
                    group: owner.group.clone(),
                    mid: mid.clone(),
                    kind: remote.section.kind,
                    codec: codec.clone(),
                    extensions,
                    encodings,
                    rtcp: RtcpParameters {
                        cname: if publication { local } else { remote }
                            .section
                            .rtcp
                            .cname
                            .clone(),
                        reduced_size: remote.section.rtcp.reduced_size,
                    },
                });
            }
        }
        if !requested.is_empty() {
            return Err(Error::Identifier);
        }
        for list in [&bindings.publications, &bindings.receptions] {
            let mut keys = BTreeSet::new();
            if list.iter().any(|b| !keys.insert(&b.key)) {
                return Err(Error::Identifier);
            }
        }
        self.accepted.retained = bindings.clone();
        self.accepted.bindings = bindings;
        self.accepted.declarable = true;
        Ok(&self.accepted.bindings)
    }

    /// Reconcile the current actual offer. Only new/replaced media need requests.
    pub fn prepare(&mut self, offer: &str, requests: &[Preparation]) -> Result<Candidate, Error> {
        self.prepare_inner(offer, requests, &[], None, None)
    }

    pub(super) fn validate_retirements(&self, mids: &[String]) -> Result<(), Error> {
        for mid in mids {
            if !self
                .accepted
                .document
                .media
                .iter()
                .any(|media| media.section.mid == *mid && media.section.kind != Kind::Application)
            {
                return Err(Error::Envelope(mid.clone()));
            }
        }
        Ok(())
    }

    pub(super) fn prepare_retirement(
        &mut self,
        offer: &str,
        mids: &[String],
    ) -> Result<Candidate, Error> {
        self.prepare_inner(offer, &[], mids, None, None)
    }

    pub(super) fn prepare_allocation(
        &mut self,
        offer: &str,
        allocations: &[Allocation],
        slots: &[crate::SlotBinding],
    ) -> Result<Candidate, Error> {
        self.prepare_inner(offer, &[], &[], Some((allocations, slots)), None)
    }

    pub(super) fn validate_allocations(&self, allocations: &[Allocation]) -> Result<(), Error> {
        for allocation in allocations {
            allocation.validate()?;
            let owner = allocation.owner();
            let binding = self
                .accepted
                .retained
                .publications
                .iter()
                .chain(&self.accepted.retained.receptions)
                .find(|binding| binding.key == owner.key);
            if let Some(binding) = binding {
                return Err(Error::Envelope(binding.mid.clone()));
            }
        }
        Ok(())
    }

    fn lower_allocations(
        &self,
        offered: &Document,
        allocations: &[Allocation],
        slots: &[crate::SlotBinding],
    ) -> Result<Vec<Preparation>, Error> {
        if allocations.len() > offered.media.len() || slots.len() > offered.media.len() {
            return Err(Error::Capacity);
        }
        self.validate_allocations(allocations)?;
        if allocations.is_empty()
            || allocations.len() != slots.len()
            || slots
                .iter()
                .map(|slot| &slot.slot)
                .collect::<BTreeSet<_>>()
                .len()
                != slots.len()
            || slots
                .iter()
                .map(|slot| &slot.mid)
                .collect::<BTreeSet<_>>()
                .len()
                != slots.len()
        {
            return Err(Error::Identifier);
        }
        let mut requests = Vec::with_capacity(allocations.len());
        for allocation in allocations {
            let slot = slots
                .iter()
                .find(|slot| slot.slot == allocation.slot)
                .ok_or(Error::Identifier)?;
            let media = &offered
                .media
                .get(slot.media_index as usize)
                .ok_or(Error::Identifier)?
                .section;
            let kind = match allocation.slot.kind() {
                crate::MediaKind::Audio => Kind::Audio,
                crate::MediaKind::Video => Kind::Video,
            };
            let publication = allocation.slot.direction() == crate::MediaDirection::SendOnly;
            if slot.mid != media.mid
                || slot.kind != allocation.slot.kind()
                || slot.direction != allocation.slot.direction()
                || media.kind != kind
                || media.stopped
                || media.direction.sends() != publication
                || media.direction.receives() == publication
            {
                return Err(Error::Envelope(slot.mid.clone()));
            }
            let owner = allocation.owner();
            if self
                .accepted
                .local
                .media
                .iter()
                .any(|old| old.section.mid == media.mid && !old.section.stopped)
            {
                return Err(Error::Envelope(media.mid.clone()));
            }
            let expected = allocation
                .format
                .as_ref()
                .map(pulsebeam_proto::state::sdp::codec_parameters)
                .transpose()
                .map_err(|_| Error::Identifier)?;
            // A source format constrains codec selection, never receiver PT/SSRCs.
            let codec = media
                .codecs
                .iter()
                .filter(|codec| {
                    expected.as_ref().is_none_or(|expected| {
                        codec.parameters.name.eq_ignore_ascii_case(&expected.name)
                            && codec.parameters.clock_rate == expected.clock_rate
                            && codec.parameters.channels == expected.channels
                            && codec.parameters.fmtp == expected.fmtp
                    })
                })
                .find_map(|codec| self.select_codec(codec).ok())
                .ok_or_else(|| Error::Envelope(media.mid.clone()))?;
            requests.push(Preparation {
                mid: media.mid.clone(),
                payload_type: codec.payload_type,
                publication: publication.then(|| owner.clone()),
                reception: (!publication).then_some(owner),
            });
        }
        Ok(requests)
    }

    fn prepare_inner(
        &mut self,
        offer: &str,
        requests: &[Preparation],
        retirements: &[String],
        allocations: Option<(&[Allocation], &[crate::SlotBinding])>,
        planned: Option<&Binding>,
    ) -> Result<Candidate, Error> {
        if self.pending.is_some() {
            return Err(Error::CandidateState);
        }
        self.validate_retirements(retirements)?;
        let offered = Document::parse(offer)?;
        if requests.len() > offered.media.len() {
            return Err(Error::Capacity);
        }
        for mid in retirements {
            if !offered.media.iter().any(|media| {
                media.section.mid == *mid
                    && media.section.kind != Kind::Application
                    && media.section.stopped
            }) {
                return Err(Error::Envelope(mid.clone()));
            }
        }
        for media in offered.media.iter().filter(|m| !m.section.stopped) {
            if !media.transport.same_peer(&self.local_transport) {
                return Err(Error::TransportChange);
            }
        }
        if self.accepted.document.media.iter().any(|old| {
            !old.section.stopped
                && !offered.media.iter().any(|new| {
                    new.section.mid == old.section.mid && new.section.kind == old.section.kind
                })
        }) {
            return Err(Error::Envelope("m-line history".into()));
        }
        // Reserve every offered sender before allocating receive identities,
        // independently of m-line processing order.
        for media in offered.media.iter().filter(|m| !m.section.stopped) {
            for (primary, repair) in &media.signaled {
                self.reserve_sender(*primary, &media.section.mid, &None, false)?;
                if let Some(repair) = repair {
                    self.reserve_sender(*repair, &media.section.mid, &None, true)?;
                }
            }
        }
        let lowered = allocations
            .map(|(allocations, slots)| self.lower_allocations(&offered, allocations, slots))
            .transpose()?;
        let requests = lowered.as_deref().unwrap_or(requests);
        let mut requested = BTreeMap::new();
        for request in requests {
            if !sdp::token(&request.mid)
                || requested.insert(request.mid.as_str(), request).is_some()
            {
                return Err(Error::Identifier);
            }
            for owner in [&request.publication, &request.reception]
                .into_iter()
                .flatten()
            {
                validate_owner(owner)?;
            }
        }
        let mut bindings = Bindings::default();
        let mut retained = self.accepted.retained.clone();
        let mut reception_plans = self.accepted.reception_plans.clone();
        let mut answered = offered.clone();
        for media in &mut answered.media {
            let mid = media.section.mid.clone();
            let prior = self
                .accepted
                .document
                .media
                .iter()
                .find(|m| m.section.mid == mid)
                .cloned();
            let request = requested.remove(mid.as_str());
            let claims_reception = request.is_some_and(|request| request.reception.is_some());
            if media.section.kind == Kind::Application {
                if request.is_some() {
                    return Err(Error::Envelope(mid));
                }
                let old = prior.ok_or(Error::TransportChange)?;
                let local = self
                    .accepted
                    .local
                    .media
                    .iter()
                    .find(|m| m.section.mid == mid)
                    .ok_or(Error::TransportChange)?;
                if local.sctp != media.sctp || old.section.stopped != media.section.stopped {
                    return Err(Error::TransportChange);
                }
                media.sctp = old.sctp;
                media.transport = self.remote_transport.clone();
                continue;
            }
            if media.section.stopped {
                if request.is_some() {
                    return Err(Error::Envelope(mid));
                }
                retained.publications.retain(|b| b.mid != mid);
                retained.receptions.retain(|b| b.mid != mid);
                reception_plans.remove(&mid);
                media.section.codecs.clear();
                media.section.encodings.clear();
                media.section.rtcp = RtcpParameters::default();
                media.section.extensions.clear();
                media.transport = self.remote_transport.clone();
                continue;
            }
            if !media.section.direction.sends()
                && !media.section.direction.receives()
                && prior.is_none()
                && request.is_none()
            {
                let codec = media
                    .section
                    .codecs
                    .iter()
                    .find_map(|c| self.select_codec(c).ok())
                    .ok_or_else(|| Error::Envelope(mid.clone()))?;
                media.section.codecs = vec![codec];
                media.section.encodings.clear();
                media.section.rtcp = RtcpParameters::default();
                media
                    .section
                    .extensions
                    .retain(|e| self.capabilities.extensions.contains(&e.uri));
                for e in &mut media.section.extensions {
                    e.direction = e.direction.inverse();
                }
                media.transport = self.remote_transport.clone();
                continue;
            }
            let publication_prior = self
                .accepted
                .retained
                .publications
                .iter()
                .find(|b| b.mid == mid)
                .cloned();
            let retained_reception = self
                .accepted
                .retained
                .receptions
                .iter()
                .find(|b| b.mid == mid)
                .cloned();
            let plan = planned.filter(|binding| binding.mid == mid).or_else(|| {
                retained_reception.as_ref().filter(|_| {
                    self.accepted.reception_plans.get(&mid) == Some(&ReceptionPlan::Authoritative)
                })
            });
            let reception_prior = retained_reception
                .as_ref()
                .filter(|_| plan.is_none() || !self.accepted.unclaimed_reception(&mid));
            media.msids = if reception_prior.is_some() {
                prior
                    .as_ref()
                    .map(|prior| prior.msids.clone())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let (codec, publication_owner, reception_owner) = if let Some(request) = request {
                let offered = media
                    .section
                    .codecs
                    .iter()
                    .find(|c| c.payload_type == request.payload_type)
                    .ok_or_else(|| Error::Envelope(mid.clone()))?;
                let mut codec = self.select_codec(offered)?;
                if !media.section.direction.sends() && request.publication.is_some()
                    || !media.section.direction.receives() && request.reception.is_some()
                {
                    return Err(Error::Envelope(mid));
                }
                if let Some(old) = publication_prior.as_ref().or(reception_prior) {
                    // A retained physical section keeps its negotiated codec envelope.
                    codec = retain_codec_envelope(codec, &old.codec)
                        .ok_or_else(|| Error::Envelope(mid.clone()))?;
                }
                (
                    codec,
                    request.publication.clone(),
                    request.reception.clone(),
                )
            } else {
                let old = prior
                    .as_ref()
                    .ok_or_else(|| Error::MissingConfiguration(mid.clone()))?;
                if old.section.stopped {
                    return Err(Error::MissingConfiguration(mid));
                }
                let old_codec = old
                    .section
                    .codecs
                    .first()
                    .ok_or_else(|| Error::MissingConfiguration(mid.clone()))?;
                let codec = media
                    .section
                    .codecs
                    .iter()
                    .find(|c| c.payload_type == old_codec.payload_type)
                    .cloned()
                    .ok_or_else(|| Error::Envelope(mid.clone()))?;
                let codec = retain_codec_envelope(self.select_codec(&codec)?, old_codec)
                    .ok_or_else(|| Error::Envelope(mid.clone()))?;
                (
                    codec,
                    publication_prior.as_ref().map(binding_owner),
                    reception_prior.map(binding_owner),
                )
            };
            let codec =
                plan.map_or_else(|| self.select_codec(&codec), |plan| Ok(plan.codec.clone()))?;
            if media.section.direction.sends() && publication_owner.is_none()
                || media.section.direction.receives() && reception_owner.is_none()
            {
                return Err(Error::MissingConfiguration(mid));
            }
            let reduced_size = plan.map_or_else(
                || {
                    prior
                        .as_ref()
                        .filter(|p| !p.section.stopped)
                        .is_some_and(|p| p.section.rtcp.reduced_size)
                },
                |plan| plan.rtcp.reduced_size,
            );
            self.check_rtcp_mode(reduced_size)?;
            if reduced_size && !media.section.rtcp.reduced_size {
                return Err(Error::Envelope(mid.clone()));
            }
            let offered_direction = media.section.direction;
            let extensions: Vec<_> = if let Some(plan) = plan {
                plan.extensions
                    .iter()
                    .map(|selected| {
                        media
                            .section
                            .extensions
                            .iter()
                            .find(|offered| {
                                offered.id == selected.id
                                    && offered.uri == selected.uri
                                    && offered.direction.receives()
                            })
                            .cloned()
                            .ok_or_else(|| Error::Envelope(mid.clone()))
                    })
                    .collect::<Result<_, _>>()?
            } else {
                media
                    .section
                    .extensions
                    .iter()
                    .filter(|extension| self.capabilities.extensions.contains(&extension.uri))
                    .cloned()
                    .collect()
            };
            if let Some(owner) = publication_owner.filter(|_| offered_direction.sends()) {
                let mut encodings = media.section.encodings.clone();
                if codec.repair.is_none() {
                    for e in &mut encodings {
                        e.repair = None;
                    }
                }
                if encodings.is_empty() {
                    encodings.push(Encoding {
                        rid: None,
                        primary: None,
                        repair: None,
                        paused: false,
                        runtime: RuntimeEncoding::default(),
                    });
                }
                // Runtime facts can outlive an SDP offer that omits SSRCs.
                if let Some(old) = &publication_prior {
                    if old.codec != codec
                        || old.encodings.len() != encodings.len()
                        || old
                            .encodings
                            .iter()
                            .zip(&encodings)
                            .any(|(a, b)| a.rid != b.rid)
                    {
                        return Err(Error::Envelope(mid.clone()));
                    }
                    for encoding in &mut encodings {
                        if let Some(observed) = old
                            .encodings
                            .iter()
                            .find(|e| same_encoding(e, encoding, old.encodings.len()))
                        {
                            if encoding.primary.is_some() && encoding.primary != observed.primary
                                || encoding.repair.is_some() && encoding.repair != observed.repair
                            {
                                return Err(Error::Collision);
                            }
                            encoding.primary = observed.primary;
                            encoding.repair = observed.repair;
                            encoding.runtime = observed.runtime.clone();
                        }
                    }
                }
                for encoding in &encodings {
                    for id in [encoding.primary, encoding.repair].into_iter().flatten() {
                        if !publication_prior.as_ref().is_some_and(|b| {
                            b.encodings
                                .iter()
                                .any(|e| e.primary == Some(id) || e.repair == Some(id))
                        }) {
                            self.reserve_sender(
                                id,
                                &mid,
                                &encoding.rid,
                                encoding.repair == Some(id),
                            )?;
                        }
                    }
                }
                let mut rtcp = media.section.rtcp.clone();
                rtcp.reduced_size = reduced_size;
                if let Some(old) = &publication_prior {
                    if rtcp.cname.is_some()
                        && old.rtcp.cname.is_some()
                        && rtcp.cname != old.rtcp.cname
                    {
                        return Err(Error::Envelope(mid.clone()));
                    }
                    if rtcp.cname.is_none() {
                        rtcp.cname.clone_from(&old.rtcp.cname);
                    }
                }
                bindings.publications.push(Binding {
                    key: owner.key,
                    group: owner.group,
                    mid: mid.clone(),
                    kind: media.section.kind,
                    codec: codec.clone(),
                    extensions: extensions
                        .iter()
                        .filter(|e| e.direction.sends())
                        .cloned()
                        .collect(),
                    encodings,
                    rtcp,
                });
            }
            if let Some(owner) = reception_owner.filter(|_| offered_direction.receives()) {
                if plan.is_some_and(|plan| plan.key != owner.key || plan.group != owner.group) {
                    return Err(Error::Envelope(mid));
                }
                if planned.is_some_and(|binding| binding.mid == mid) {
                    reception_plans.insert(mid.clone(), ReceptionPlan::Authoritative);
                } else if claims_reception && self.accepted.unclaimed_reception(&mid) {
                    reception_plans.remove(&mid);
                }
                let encodings = if let Some(plan) = plan {
                    if reception_prior.is_none() {
                        for identity in plan
                            .encodings
                            .iter()
                            .flat_map(|encoding| [encoding.primary, encoding.repair])
                            .flatten()
                        {
                            self.identities.reserve(identity)?;
                        }
                    }
                    plan.encodings.clone()
                } else if let Some(old) = reception_prior {
                    old.encodings.clone()
                } else {
                    vec![Encoding {
                        rid: None,
                        primary: Some(self.allocate()?),
                        repair: if codec.repair.is_some() {
                            Some(self.allocate()?)
                        } else {
                            None
                        },
                        paused: false,
                        runtime: RuntimeEncoding::default(),
                    }]
                };
                if reception_prior.is_none() {
                    media.msids = encodings
                        .first()
                        .and_then(|encoding| encoding.primary)
                        .map(|primary| vec![format!("pulsebeam-{mid} output-{primary}")])
                        .unwrap_or_default();
                }
                bindings.receptions.push(Binding {
                    key: owner.key,
                    group: owner.group,
                    mid: mid.clone(),
                    kind: media.section.kind,
                    codec: codec.clone(),
                    extensions: extensions
                        .iter()
                        .filter(|e| e.direction.receives())
                        .cloned()
                        .collect(),
                    rtcp: plan
                        .or(reception_prior)
                        .as_ref()
                        .map(|b| b.rtcp.clone())
                        .unwrap_or_else(|| RtcpParameters {
                            cname: encodings
                                .first()
                                .and_then(|e| e.primary)
                                .map(|primary| format!("pulsebeam-{primary:08x}")),
                            reduced_size,
                        }),
                    encodings,
                });
            }
            media.section.codecs = vec![codec];
            media.section.extensions = extensions
                .into_iter()
                .map(|mut e| {
                    e.direction = e.direction.inverse();
                    e
                })
                .collect();
            media.section.direction = offered_direction.inverse();
            media.section.rtcp = bindings
                .receptions
                .iter()
                .find(|b| b.mid == mid)
                .map(|b| b.rtcp.clone())
                .unwrap_or(RtcpParameters {
                    cname: None,
                    reduced_size,
                });
            media.section.encodings = bindings
                .receptions
                .iter()
                .find(|b| b.mid == mid)
                .map(|b| b.encodings.clone())
                .unwrap_or_default();
            media.transport = self.remote_transport.clone();
        }
        if !requested.is_empty() {
            return Err(Error::Identifier);
        }
        for list in [&bindings.publications, &bindings.receptions] {
            let mut keys = BTreeSet::new();
            if list.iter().any(|b| !keys.insert(&b.key)) {
                return Err(Error::Identifier);
            }
        }
        answered.origin.clone_from(&self.accepted.document.origin);
        answered.version = self
            .accepted
            .document
            .version
            .checked_add(1)
            .ok_or(Error::Capacity)?;
        answered.ice_lite = self.accepted.document.ice_lite;
        let answer = render(&answered, &bindings)?;
        if answer.len() > super::MAX_SDP_BYTES {
            return Err(Error::Capacity);
        }
        self.next_candidate = self.next_candidate.checked_add(1).ok_or(Error::Capacity)?;
        let id = CandidateId(self.next_candidate);
        for (active, history) in [
            (&bindings.publications, &mut retained.publications),
            (&bindings.receptions, &mut retained.receptions),
        ] {
            for binding in active {
                history.retain(|b| b.mid != binding.mid);
                history.push(binding.clone());
            }
        }
        self.pending = Some((
            id,
            Accepted {
                document: answered,
                bindings: bindings.clone(),
                retained,
                local: offered,
                declarable: true,
                reception_plans,
            },
        ));
        Ok(Candidate {
            id,
            answer,
            bindings,
        })
    }

    /// Report reality, including cancellation after successful application.
    pub fn finish(&mut self, id: CandidateId, outcome: ApplyOutcome) -> Result<(), Error> {
        if self.pending.as_ref().map(|p| p.0) != Some(id) {
            return Err(Error::CandidateState);
        }
        let (_, mut pending) = self.pending.take().ok_or(Error::CandidateState)?;
        if outcome != ApplyOutcome::NotApplied {
            pending.declarable = outcome == ApplyOutcome::Applied;
            for binding in self
                .accepted
                .retained
                .publications
                .iter()
                .chain(&self.accepted.retained.receptions)
            {
                let replaced_bootstrap = self.accepted.unclaimed_reception(&binding.mid)
                    && !pending.unclaimed_reception(&binding.mid)
                    && pending
                        .retained
                        .receptions
                        .iter()
                        .find(|next| next.mid == binding.mid)
                        .is_some_and(|next| next.encodings != binding.encodings);
                if replaced_bootstrap
                    || pending
                        .document
                        .media
                        .iter()
                        .any(|m| m.section.mid == binding.mid && m.section.stopped)
                {
                    for id in binding
                        .encodings
                        .iter()
                        .flat_map(|e| [e.primary, e.repair])
                        .flatten()
                    {
                        self.identities.retire(id)?;
                    }
                }
            }
            self.accepted = pending;
        }
        Ok(())
    }

    /// None after an applied-but-cancelled candidate, even though SDP advanced.
    pub fn bindings(&self) -> Option<&Bindings> {
        self.accepted.declarable.then_some(&self.accepted.bindings)
    }

    /// Actual applied physical facts, including cancelled work. Not an admission.
    pub fn applied_bindings(&self) -> &Bindings {
        &self.accepted.bindings
    }

    pub fn stopped(&self, mid: &str) -> bool {
        self.accepted
            .local
            .media
            .iter()
            .any(|media| media.section.mid == mid && media.section.stopped)
    }

    pub(super) fn prepare_abandoned_allocation(
        &mut self,
        offer: &str,
        mids: &[String],
    ) -> Result<Candidate, Error> {
        let offered = Document::parse(offer)?;
        for mid in mids {
            if offered
                .media
                .iter()
                .any(|media| media.section.mid == *mid && !media.section.stopped)
            {
                return Err(Error::Envelope(mid.clone()));
            }
        }
        self.prepare(offer, &[])
    }

    /// Finalize missing sender identities after applying the candidate. A failed
    /// batch changes neither accepted facts nor the declaration's completeness.
    pub fn observe_sender(
        &mut self,
        mid: &str,
        observations: &[Encoding],
        rtcp: &RtcpParameters,
    ) -> Result<&Bindings, Error> {
        self.observe_sender_inner(mid, observations, rtcp, true)
    }

    /// Retain partial runtime facts without treating missing values as identities.
    /// Known applied SDP facts fill absent observations, never conflicting ones.
    pub fn observe_sender_facts(
        &mut self,
        mid: &str,
        observations: &[Encoding],
        rtcp: &RtcpParameters,
    ) -> Result<&Bindings, Error> {
        self.observe_sender_inner(mid, observations, rtcp, false)
    }

    fn observe_sender_inner(
        &mut self,
        mid: &str,
        observations: &[Encoding],
        rtcp: &RtcpParameters,
        complete: bool,
    ) -> Result<&Bindings, Error> {
        if self.pending.is_some() || complete && !self.accepted.declarable {
            return Err(Error::CandidateState);
        }
        let index = self
            .accepted
            .bindings
            .publications
            .iter()
            .position(|b| b.mid == mid)
            .ok_or(Error::Observation)?;
        let binding = self
            .accepted
            .bindings
            .publications
            .get(index)
            .ok_or(Error::Observation)?;
        if observations.len() != binding.encodings.len()
            || rtcp
                .cname
                .as_deref()
                .is_some_and(|cname| !sdp::cname(cname))
            || observations.iter().any(|observed| {
                observed.rid.as_deref().is_some_and(|rid| !sdp::token(rid))
                    || observed
                        .runtime
                        .scalability_mode
                        .as_deref()
                        .is_some_and(|mode| !sdp::token(mode))
            })
        {
            return Err(Error::Observation);
        }
        let observations = observations
            .iter()
            .map(|observed| {
                let mut observed = observed.clone();
                if !complete {
                    let old = binding
                        .encodings
                        .iter()
                        .find(|old| same_encoding(old, &observed, binding.encodings.len()))
                        .or_else(|| {
                            if binding.encodings.len() == 1 && observed.rid.is_none() {
                                binding.encodings.first()
                            } else {
                                None
                            }
                        })
                        .ok_or(Error::Observation)?;
                    observed.rid = observed.rid.or_else(|| old.rid.clone());
                    observed.primary = observed.primary.or(old.primary);
                    observed.repair = observed.repair.or(old.repair);
                }
                Ok(observed)
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let mut rtcp = rtcp.clone();
        if rtcp.cname.is_none() {
            rtcp.cname.clone_from(&binding.rtcp.cname);
        }
        if complete && rtcp.cname.is_none()
            || rtcp.reduced_size != binding.rtcp.reduced_size
            || binding.rtcp.cname.is_some() && rtcp.cname != binding.rtcp.cname
        {
            return Err(Error::Observation);
        }
        let mut ids = BTreeSet::new();
        let mut rids = BTreeSet::new();
        for observed in &observations {
            let old = binding
                .encodings
                .iter()
                .find(|e| same_encoding(e, observed, binding.encodings.len()))
                .ok_or(Error::Observation)?;
            if observed.rid.as_ref().is_some_and(|rid| !rids.insert(rid))
                || complete && observed.primary.is_none()
                || (binding.codec.repair.is_none() && observed.repair.is_some())
                || (complete && binding.codec.repair.is_some() && observed.repair.is_none())
                || old.primary.is_some() && old.primary != observed.primary
                || old.repair.is_some() && old.repair != observed.repair
                || observed
                    .runtime
                    .max_framerate
                    .is_some_and(|v| !v.is_finite() || v <= 0.0)
            {
                return Err(Error::Observation);
            }
            let local = self
                .accepted
                .local
                .media
                .iter()
                .find(|m| m.section.mid == mid)
                .ok_or(Error::Observation)?;
            if [observed.primary, observed.repair]
                .into_iter()
                .flatten()
                .any(|id| {
                    local
                        .cnames
                        .get(&id)
                        .is_some_and(|cname| Some(cname) != rtcp.cname.as_ref())
                })
            {
                return Err(Error::Observation);
            }
            if !local.signaled.is_empty()
                && observed.primary.is_some()
                && !observed.primary.is_some_and(|primary| {
                    local.signaled.get(&primary).is_some_and(|repair| {
                        binding.codec.repair.is_none() || *repair == observed.repair
                    })
                })
            {
                return Err(Error::Observation);
            }
            for (id, repair) in [(observed.primary, false), (observed.repair, true)] {
                if let Some(id) = id {
                    let same_owner = self.identities.same_sender(id, mid, &observed.rid, repair);
                    if self.identities.retired(id)
                        || !ids.insert(id)
                        || self.identities.contains(id)
                            && old.primary != Some(id)
                            && old.repair != Some(id)
                            && !same_owner
                    {
                        return Err(Error::Collision);
                    }
                }
            }
        }
        self.identities
            .reserve_senders(observations.iter().flat_map(|observed| {
                [(observed.primary, false), (observed.repair, true)]
                    .into_iter()
                    .filter_map(move |(id, repair)| id.map(|id| (id, mid, &observed.rid, repair)))
            }))?;
        let binding = self
            .accepted
            .bindings
            .publications
            .get_mut(index)
            .ok_or(Error::Observation)?;
        binding.encodings = observations.to_vec();
        binding.rtcp = rtcp.clone();
        if let Some(retained) = self
            .accepted
            .retained
            .publications
            .iter_mut()
            .find(|b| b.mid == mid)
        {
            retained.encodings = observations.to_vec();
            retained.rtcp = rtcp;
        }
        Ok(&self.accepted.bindings)
    }

    fn select_codec(&self, offered: &Codec) -> Result<Codec, Error> {
        let supported = self
            .capabilities
            .codecs
            .iter()
            .find(|c| {
                c.parameters
                    .name
                    .eq_ignore_ascii_case(&offered.parameters.name)
                    && c.parameters.clock_rate == offered.parameters.clock_rate
                    && c.parameters.channels == offered.parameters.channels
                    && c.parameters.fmtp == offered.parameters.fmtp
            })
            .ok_or_else(|| Error::Envelope("codec".into()))?;
        let mut codec = offered.clone();
        if !supported.repair {
            codec.repair = None;
        }
        codec.parameters.feedback = offered
            .parameters
            .feedback
            .intersection(&supported.parameters.feedback)
            .cloned()
            .collect();
        Ok(codec)
    }
    fn check_rtcp_mode(&self, reduced_size: bool) -> Result<(), Error> {
        let mode = if reduced_size {
            RtcpMode::ReducedSize
        } else {
            RtcpMode::Compound
        };
        if self.capabilities.rtcp_modes.contains(&mode) {
            Ok(())
        } else {
            Err(Error::Envelope("RTCP mode".into()))
        }
    }

    fn reserve_sender(
        &mut self,
        id: u32,
        mid: &str,
        rid: &Option<String>,
        repair: bool,
    ) -> Result<(), Error> {
        self.identities.reserve_sender(id, mid, rid, repair)
    }
    fn allocate(&mut self) -> Result<u32, Error> {
        // libwebrtc reserves zero in its receiver frame-transformer API. Keep
        // local allocations nonzero; observed SSRCs retain their full u32 value.
        // One extra probe accounts for skipping zero.
        for _ in 0..self.identities.len().saturating_add(2) {
            self.next_identity = self.next_identity.wrapping_add(1);
            let id = self.next_identity;
            if id != 0 && !self.identities.contains(id) {
                self.identities.reserve(id)?;
                return Ok(id);
            }
        }
        Err(Error::Capacity)
    }
}

fn same_encoding(accepted: &Encoding, observed: &Encoding, count: usize) -> bool {
    accepted.rid == observed.rid
        && (accepted.rid.is_some() || count == 1 || accepted.primary == observed.primary)
}

fn retain_codec_envelope(mut selected: Codec, prior: &Codec) -> Option<Codec> {
    if !prior
        .parameters
        .feedback
        .is_subset(&selected.parameters.feedback)
    {
        return None;
    }
    selected
        .parameters
        .feedback
        .clone_from(&prior.parameters.feedback);
    if prior.repair.is_none() {
        selected.repair = None;
    }
    (selected == *prior).then_some(selected)
}

fn validate_owner(owner: &MediaOwner) -> Result<(), Error> {
    if owner.key.0.is_empty()
        || owner.key.0.len() > 256
        || owner
            .group
            .as_ref()
            .is_some_and(|g| g.0.is_empty() || g.0.len() > 256)
    {
        return Err(Error::Identifier);
    }
    Ok(())
}

fn binding_owner(b: &Binding) -> MediaOwner {
    MediaOwner {
        key: b.key.clone(),
        group: b.group.clone(),
    }
}
fn active_transport(document: &Document) -> Result<Transport, Error> {
    document
        .media
        .iter()
        .find(|m| !m.section.stopped)
        .map(|m| m.transport.clone())
        .ok_or(Error::TransportChange)
}

fn validate_capabilities(caps: &Capabilities) -> Result<(), Error> {
    if caps.codecs.len() > 128 || caps.extensions.len() > 256 {
        return Err(Error::Capacity);
    }
    for capability in &caps.codecs {
        let codec = &capability.parameters;
        if !sdp::token(&codec.name)
            || codec.clock_rate == 0
            || codec.channels == Some(0)
            || codec.fmtp.len() > 64
            || codec.feedback.len() > 32
        {
            return Err(Error::Identifier);
        }
        if codec
            .fmtp
            .iter()
            .any(|(k, v)| !sdp::token(k) || v.contains(['\r', '\n', ';']) || v.len() > 1024)
            || codec
                .feedback
                .iter()
                .any(|v| v.is_empty() || v.contains(['\r', '\n']) || v.len() > 256)
        {
            return Err(Error::Identifier);
        }
    }
    if caps
        .extensions
        .iter()
        .any(|s| s.is_empty() || s.len() > 1024 || s.chars().any(char::is_whitespace))
    {
        return Err(Error::Identifier);
    }
    Ok(())
}

fn render(document: &Document, bindings: &Bindings) -> Result<String, Error> {
    let mut origin = document.origin.clone();
    *origin.get_mut(2).ok_or(Error::Malformed("origin"))? = document.version.to_string();
    let mids = document
        .bundle
        .iter()
        .filter(|mid| {
            document
                .media
                .iter()
                .any(|m| m.section.mid == **mid && !m.section.stopped)
        })
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let mut lines = vec![
        "v=0".into(),
        format!("o={}", origin.join(" ")),
        "s=-".into(),
        "t=0 0".into(),
        format!("a=group:BUNDLE {mids}"),
        "a=msid-semantic: WMS *".into(),
    ];
    if document.ice_lite {
        lines.push("a=ice-lite".into());
    }
    for media in &document.media {
        let section = &media.section;
        let kind = match section.kind {
            Kind::Audio => "audio",
            Kind::Video => "video",
            Kind::Application => "application",
        };
        let formats = if section.kind == Kind::Application || section.stopped {
            media.formats.join(" ")
        } else {
            section
                .codecs
                .iter()
                .flat_map(|c| {
                    std::iter::once(c.payload_type.to_string())
                        .chain(c.repair.iter().map(|r| r.payload_type.to_string()))
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        let port = if section.stopped { 0 } else { 9 };
        lines.extend([
            format!("m={kind} {port} {} {formats}", media.proto),
            "c=IN IP4 0.0.0.0".into(),
            format!("a=mid:{}", section.mid),
        ]);
        media.transport.render(&mut lines);
        if section.kind == Kind::Application {
            lines.extend(media.sctp.iter().cloned());
            continue;
        }
        lines.push(format!("a={}", section.direction.token()));
        if media.extmap_allow_mixed {
            // DD structures can exceed the one-byte extension value limit.
            lines.push("a=extmap-allow-mixed".into());
        }
        lines.push("a=rtcp-mux".into());
        if section.rtcp.reduced_size {
            lines.push("a=rtcp-rsize".into());
        }
        if section.stopped {
            continue;
        }
        for codec in &section.codecs {
            let p = &codec.parameters;
            let channels = p.channels.map(|c| format!("/{c}")).unwrap_or_default();
            // Chromium's encoder dispatch requires canonical video codec spelling.
            let name = match p.name.as_str() {
                "h264" => "H264",
                "vp8" => "VP8",
                "vp9" => "VP9",
                "av1" => "AV1",
                name => name,
            };
            lines.push(format!(
                "a=rtpmap:{} {}/{}{channels}",
                codec.payload_type, name, p.clock_rate
            ));
            if !p.fmtp.is_empty() {
                lines.push(format!(
                    "a=fmtp:{} {}",
                    codec.payload_type,
                    p.fmtp
                        .iter()
                        .map(|(k, v)| if v.is_empty() {
                            k.clone()
                        } else {
                            format!("{k}={v}")
                        })
                        .collect::<Vec<_>>()
                        .join(";")
                ));
            }
            for fb in &p.feedback {
                lines.push(format!("a=rtcp-fb:{} {fb}", codec.payload_type));
            }
            if let Some(repair) = &codec.repair {
                lines.extend([
                    format!("a=rtpmap:{} rtx/{}", repair.payload_type, repair.clock_rate),
                    format!("a=fmtp:{} apt={}", repair.payload_type, codec.payload_type),
                ]);
            }
        }
        for extension in &section.extensions {
            lines.push(format!(
                "a=extmap:{}/{} {}",
                extension.id,
                extension.direction.token(),
                extension.uri
            ));
        }
        // MSID names the physical receiver, independently of source SSRC replacement.
        let msids = if section
            .encodings
            .iter()
            .any(|encoding| encoding.primary.is_some())
        {
            media.msids.as_slice()
        } else {
            &[]
        };
        for msid in msids {
            lines.push(format!("a=msid:{msid}"));
        }
        for encoding in &section.encodings {
            if let Some(primary) = encoding.primary {
                let cname = section
                    .rtcp
                    .cname
                    .as_deref()
                    .filter(|cname| sdp::cname(cname))
                    .ok_or_else(|| Error::Envelope(section.mid.clone()))?;
                lines.push(format!("a=ssrc:{primary} cname:{cname}"));
                for msid in msids {
                    lines.push(format!("a=ssrc:{primary} msid:{msid}"));
                }
                if let Some(repair) = encoding.repair {
                    lines.extend([
                        format!("a=ssrc:{repair} cname:{cname}"),
                        format!("a=ssrc-group:FID {primary} {repair}"),
                    ]);
                    for msid in msids {
                        lines.push(format!("a=ssrc:{repair} msid:{msid}"));
                    }
                }
            }
        }
        if let Some(publication) = bindings.publications.iter().find(|b| b.mid == section.mid) {
            let rids: Vec<_> = publication
                .encodings
                .iter()
                .filter_map(|e| e.rid.as_ref().map(|r| (r, e.paused)))
                .collect();
            for (rid, _) in &rids {
                lines.push(format!("a=rid:{rid} recv"));
            }
            if !rids.is_empty() {
                lines.push(format!(
                    "a=simulcast:recv {}",
                    rids.iter()
                        .map(|(r, paused)| if *paused {
                            format!("~{r}")
                        } else {
                            (*r).clone()
                        })
                        .collect::<Vec<_>>()
                        .join(";")
                ));
            }
        }
    }
    Ok(format!("{}\r\n", lines.join("\r\n")))
}
