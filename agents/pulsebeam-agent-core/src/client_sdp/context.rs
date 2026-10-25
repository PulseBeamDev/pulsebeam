use super::sdp::{self, Document, Transport};
use super::{
    Binding, Bindings, Capabilities, Codec, Encoding, Error, Kind, Limits, MediaOwner, Preparation,
    RuntimeEncoding,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
};

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

#[derive(Clone)]
struct Accepted {
    document: Document,
    bindings: Bindings,
    retained: Bindings,
    local: Document,
    declarable: bool,
}

/// One context per PeerConnection. No ambient entropy, network or async state.
pub struct Context {
    local_transport: Transport,
    remote_transport: Transport,
    capabilities: Capabilities,
    limits: Limits,
    accepted: Accepted,
    pending: Option<(CandidateId, Accepted)>,
    used: BTreeSet<NonZeroU32>,
    sender_identities: BTreeMap<NonZeroU32, (String, Option<String>, bool)>,
    retired: BTreeSet<NonZeroU32>,
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
        if limits.max_sections == 0
            || limits.max_sections > 256
            || limits.max_encodings == 0
            || limits.max_encodings > 1024
            || limits.max_identity_history == 0
            || limits.max_identity_history > 65536
        {
            return Err(Error::Capacity);
        }
        validate_capabilities(&capabilities)?;
        let local = Document::parse(bootstrap_offer, limits)?;
        let remote = Document::parse(applied_answer, limits)?;
        if local.media.len() != remote.media.len()
            || local
                .media
                .iter()
                .zip(&remote.media)
                .any(|(a, b)| a.section.mid != b.section.mid || a.section.kind != b.section.kind)
        {
            return Err(Error::Malformed("bootstrap alignment"));
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
        if used.len() > limits.max_identity_history {
            return Err(Error::Capacity);
        }
        Ok(Self {
            local_transport,
            remote_transport,
            capabilities,
            limits,
            accepted: Accepted {
                document: remote,
                bindings: Bindings::default(),
                retained: Bindings::default(),
                local,
                declarable: false,
            },
            pending: None,
            used,
            sender_identities,
            retired: BTreeSet::new(),
            next_identity: seed,
            next_candidate: 0,
        })
    }

    /// Correlate media already applied during bootstrap without applying another
    /// answer. Call once before preparing subsequent candidates. SCTP-only
    /// bootstrap needs no adoption. Keys remain caller-owned even for bootstrap.
    pub fn adopt_bootstrap_media(&mut self, requests: &[Preparation]) -> Result<&Bindings, Error> {
        if self.next_candidate != 0 || self.pending.is_some() || self.accepted.declarable {
            return Err(Error::CandidateState);
        }
        if requests.len() > self.limits.max_sections {
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
        if self.pending.is_some() {
            return Err(Error::CandidateState);
        }
        if requests.len() > self.limits.max_sections {
            return Err(Error::Capacity);
        }
        let offered = Document::parse(offer, self.limits)?;
        for media in offered.media.iter().filter(|m| !m.section.stopped) {
            if !media.transport.same_peer(&self.local_transport) {
                return Err(Error::TransportChange);
            }
        }
        if self.accepted.document.media.iter().any(|old| {
            !offered.media.iter().any(|new| {
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
                media.section.codecs.clear();
                media.section.encodings.clear();
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
            let reception_prior = self
                .accepted
                .retained
                .receptions
                .iter()
                .find(|b| b.mid == mid)
                .cloned();
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
                if let Some(old) = publication_prior.as_ref().or(reception_prior.as_ref()) {
                    // Explicit replacement needs a fresh physical section.
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
                    reception_prior.as_ref().map(binding_owner),
                )
            };
            let codec = self.select_codec(&codec)?;
            if media.section.direction.sends() && publication_owner.is_none()
                || media.section.direction.receives() && reception_owner.is_none()
            {
                return Err(Error::MissingConfiguration(mid));
            }
            let offered_direction = media.section.direction;
            let extensions: Vec<_> = media
                .section
                .extensions
                .iter()
                .filter(|e| self.capabilities.extensions.contains(&e.uri))
                .cloned()
                .collect();
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
                });
            }
            if let Some(owner) = reception_owner.filter(|_| offered_direction.receives()) {
                let encodings = if let Some(old) = &reception_prior {
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
        let total: usize = bindings
            .publications
            .iter()
            .chain(&bindings.receptions)
            .map(|b| b.encodings.len())
            .sum();
        if total > self.limits.max_encodings {
            return Err(Error::Capacity);
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
                if pending
                    .document
                    .media
                    .iter()
                    .any(|m| m.section.mid == binding.mid && m.section.stopped)
                {
                    self.retired.extend(
                        binding
                            .encodings
                            .iter()
                            .flat_map(|e| [e.primary, e.repair])
                            .flatten(),
                    );
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

    /// Finalize missing sender identities after applying the candidate. A failed
    /// batch changes neither accepted facts nor the declaration's completeness.
    pub fn observe_sender(
        &mut self,
        mid: &str,
        observations: &[Encoding],
    ) -> Result<&Bindings, Error> {
        if self.pending.is_some() || !self.accepted.declarable {
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
        if observations.len() != binding.encodings.len() {
            return Err(Error::Observation);
        }
        let mut ids = BTreeSet::new();
        let mut rids = BTreeSet::new();
        for observed in observations {
            let old = binding
                .encodings
                .iter()
                .find(|e| same_encoding(e, observed, binding.encodings.len()))
                .ok_or(Error::Observation)?;
            if observed.rid.as_ref().is_some_and(|rid| !rids.insert(rid))
                || observed.primary.is_none()
                || binding.codec.repair.is_some() != observed.repair.is_some()
                || old.primary.is_some() && old.primary != observed.primary
                || old.repair.is_some() && old.repair != observed.repair
                || observed
                    .runtime
                    .max_framerate
                    .is_some_and(|v| !v.is_finite() || v <= 0.0)
                || observed
                    .runtime
                    .scalability_mode
                    .as_ref()
                    .is_some_and(|s| !sdp::token(s))
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
            if !local.signaled.is_empty()
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
                    let same_owner = self.sender_identities.get(&id).is_some_and(|owner| {
                        owner.0 == mid
                            && owner.2 == repair
                            && (owner.1.is_none() || owner.1 == observed.rid)
                    });
                    if !ids.insert(id)
                        || self.used.contains(&id)
                            && old.primary != Some(id)
                            && old.repair != Some(id)
                            && !same_owner
                    {
                        return Err(Error::Collision);
                    }
                }
            }
        }
        if self.used.union(&ids).count() > self.limits.max_identity_history {
            return Err(Error::Capacity);
        }
        for observed in observations {
            for (id, repair) in [(observed.primary, false), (observed.repair, true)] {
                if let Some(id) = id {
                    self.reserve_sender(id, mid, &observed.rid, repair)?;
                }
            }
        }
        self.accepted
            .bindings
            .publications
            .get_mut(index)
            .ok_or(Error::Observation)?
            .encodings = observations.to_vec();
        if let Some(retained) = self
            .accepted
            .retained
            .publications
            .iter_mut()
            .find(|b| b.mid == mid)
        {
            retained.encodings = observations.to_vec();
        }
        Ok(&self.accepted.bindings)
    }

    fn select_codec(&self, offered: &Codec) -> Result<Codec, Error> {
        let supported = self
            .capabilities
            .codecs
            .iter()
            .find(|c| {
                c.name.eq_ignore_ascii_case(&offered.parameters.name)
                    && c.clock_rate == offered.parameters.clock_rate
                    && c.channels == offered.parameters.channels
                    && c.fmtp == offered.parameters.fmtp
            })
            .ok_or_else(|| Error::Envelope("codec".into()))?;
        let mut codec = offered.clone();
        if !self.capabilities.repair {
            codec.repair = None;
        }
        codec.parameters.feedback = offered
            .parameters
            .feedback
            .intersection(&supported.feedback)
            .cloned()
            .collect();
        Ok(codec)
    }
    fn reserve_sender(
        &mut self,
        id: NonZeroU32,
        mid: &str,
        rid: &Option<String>,
        repair: bool,
    ) -> Result<(), Error> {
        if self.retired.contains(&id) {
            return Err(Error::Collision);
        }
        let owner = (mid.to_owned(), rid.clone(), repair);
        if let Some(old) = self.sender_identities.get_mut(&id) {
            if old.0 != mid || old.2 != repair || old.1.is_some() && rid.is_some() && old.1 != *rid
            {
                return Err(Error::Collision);
            }
            if rid.is_some() {
                old.1.clone_from(rid);
            }
            return Ok(());
        }
        self.reserve(id)?;
        self.sender_identities.insert(id, owner);
        Ok(())
    }
    fn reserve(&mut self, id: NonZeroU32) -> Result<(), Error> {
        if self.used.contains(&id) {
            return Err(Error::Collision);
        }
        if self.used.len() >= self.limits.max_identity_history {
            return Err(Error::Capacity);
        }
        self.used.insert(id);
        Ok(())
    }
    fn allocate(&mut self) -> Result<NonZeroU32, Error> {
        // A bounded probe includes one extra slot for zero. Sequential allocation
        // is deterministic; the caller supplies a connection-specific seed.
        for _ in 0..self.limits.max_identity_history.saturating_add(2) {
            self.next_identity = self.next_identity.wrapping_add(1);
            if let Some(id) = NonZeroU32::new(self.next_identity) {
                if !self.used.contains(&id) {
                    self.reserve(id)?;
                    return Ok(id);
                }
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
    if caps.codecs.is_empty() || caps.codecs.len() > 128 || caps.extensions.len() > 256 {
        return Err(Error::Capacity);
    }
    for codec in &caps.codecs {
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
        lines.push("a=rtcp-mux".into());
        if section.stopped {
            continue;
        }
        for codec in &section.codecs {
            let p = &codec.parameters;
            let channels = p.channels.map(|c| format!("/{c}")).unwrap_or_default();
            lines.push(format!(
                "a=rtpmap:{} {}/{}{channels}",
                codec.payload_type, p.name, p.clock_rate
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
        // MSID names the transport output, never the currently selected source.
        let msid = section
            .encodings
            .first()
            .and_then(|e| e.primary)
            .map(|primary| format!("pulsebeam-{} output-{primary}", section.mid));
        if let Some(msid) = &msid {
            lines.push(format!("a=msid:{msid}"));
        }
        for encoding in &section.encodings {
            if let Some(primary) = encoding.primary {
                lines.push(format!("a=ssrc:{primary} cname:pulsebeam"));
                if let Some(msid) = &msid {
                    lines.push(format!("a=ssrc:{primary} msid:{msid}"));
                }
                if let Some(repair) = encoding.repair {
                    lines.extend([
                        format!("a=ssrc:{repair} cname:pulsebeam"),
                        format!("a=ssrc-group:FID {primary} {repair}"),
                    ]);
                    if let Some(msid) = &msid {
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
