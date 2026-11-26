use super::*;

/// One physical observation request under a persistent named resource owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MediaObservationId {
    owner: TrackOwner,
    request: TrackOwner,
}

impl MediaObservationId {
    pub const fn owner(self) -> u64 {
        self.owner.id()
    }

    pub const fn request(self) -> u64 {
        self.request.id()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaObservationTarget {
    pub generation: crate::Generation,
    /// The existing receiver-owned resource lifetime, not an allocation operation.
    pub reference: u64,
    pub(crate) offer: Option<crate::native_state::Offer>,
    pub source: String,
    pub binding: crate::SlotBinding,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaObservationControl {
    pub(crate) effect: Option<crate::OperationId>,
    pub observation: MediaObservationId,
    pub target: Option<MediaObservationTarget>,
    pub closed: bool,
}

#[derive(Default)]
pub(super) struct MediaObservation {
    source: Option<String>,
    admission: Option<crate::AdmittedMedia>,
    control: Option<MediaObservationControl>,
    completed: bool,
    attached: bool,
    closed: bool,
}

impl TrackResources {
    pub(crate) fn begin_media_observation(
        &mut self,
        owner: TrackOwner,
    ) -> Result<MediaObservationId, TrackResourceError> {
        if matches!(self.audio_views.get(&owner), Some(AudioView::Policy(_))) {
            return Err(TrackResourceError::WrongKind);
        }
        if !self.video.contains_key(&owner) && !self.audio_views.contains_key(&owner) {
            return Err(TrackResourceError::Released);
        }
        let id = MediaObservationId {
            owner,
            request: self.new_owner()?,
        };
        self.observations.insert(id, MediaObservation::default());
        Ok(id)
    }

    pub(crate) fn begin_audio_source_observation(
        &mut self,
        owner: TrackOwner,
        source: String,
    ) -> Result<MediaObservationId, TrackResourceError> {
        validate_identifier("audio publication_id", &source, 256, true)?;
        if !self.audio_views.contains_key(&owner) {
            return Err(if self.video.contains_key(&owner) {
                TrackResourceError::WrongKind
            } else {
                TrackResourceError::Released
            });
        }
        let id = MediaObservationId {
            owner,
            request: self.new_owner()?,
        };
        self.observations.insert(
            id,
            MediaObservation {
                source: Some(source),
                ..Default::default()
            },
        );
        Ok(id)
    }

    pub(crate) fn begin_admitted_media_observation(
        &mut self,
        admission: crate::AdmittedMedia,
    ) -> Result<MediaObservationId, TrackResourceError> {
        let owner = admission.owner();
        let start = MediaObservationId {
            owner,
            request: TrackOwner(0),
        };
        let end = MediaObservationId {
            owner,
            request: TrackOwner(u64::MAX),
        };
        let mut count = 0_usize;
        for (_, observation) in self.observations.range(start..=end) {
            if !observation.closed && observation.admission.as_ref() == Some(&admission) {
                return Err(TrackResourceError::AlreadyOwned);
            }
            count = count.saturating_add(1);
        }
        if count >= pulsebeam_proto::state::validation::MAX_SELECTOR_MATCHES {
            return Err(TrackResourceError::SelectorObservationCapacityExceeded);
        }
        let id = MediaObservationId {
            owner,
            request: self.new_owner()?,
        };
        self.observations.insert(
            id,
            MediaObservation {
                admission: Some(admission),
                ..Default::default()
            },
        );
        Ok(id)
    }

    pub(crate) fn observation_admission(
        &self,
        id: MediaObservationId,
    ) -> Option<&crate::AdmittedMedia> {
        self.observations.get(&id)?.admission.as_ref()
    }

    pub(crate) fn audio_observation_source(&self, id: MediaObservationId) -> Option<&str> {
        self.observations.get(&id)?.source.as_deref()
    }

    pub(crate) fn end_audio_source_observations(&mut self, source: &str) {
        for observation in self.observations.values_mut() {
            if observation.source.as_deref() == Some(source) {
                observation.closed = true;
            }
        }
    }

    pub(crate) fn observation_requests(&self) -> Vec<MediaObservationId> {
        self.observations.keys().copied().collect()
    }

    pub(crate) fn video_observation_selector(
        &self,
        id: MediaObservationId,
    ) -> Option<&TrackSelector> {
        let view = self.video.get(&id.owner)?;
        view.observation.visible.then_some(&view.selector)
    }

    pub(crate) fn audio_observation_selector(
        &self,
        id: MediaObservationId,
    ) -> Option<&TrackSelector> {
        match self.audio_views.get(&id.owner)? {
            AudioView::Named { selector, .. } => Some(selector),
            AudioView::Policy(_) => None,
        }
    }

    pub(crate) fn observation_control(
        &self,
        id: MediaObservationId,
        target: Option<MediaObservationTarget>,
    ) -> Option<MediaObservationControl> {
        let observation = self.observations.get(&id)?;
        let previous = observation
            .control
            .as_ref()
            .and_then(|control| control.target.as_ref());
        // Collection observations borrow one admission. Once targeted, even
        // an unfinished attachment must not silently move to a new admission.
        let pinned =
            observation.attached || (observation.admission.is_some() && previous.is_some());
        let closed = observation.closed || pinned && previous != target.as_ref();
        Some(MediaObservationControl {
            effect: None,
            observation: id,
            target: if closed { None } else { target },
            closed,
        })
    }

    pub(crate) fn observation_control_changed(&self, control: &MediaObservationControl) -> bool {
        self.observations
            .get(&control.observation)
            .is_some_and(|observation| {
                observation.control.as_ref().is_none_or(|previous| {
                    previous.target != control.target || previous.closed != control.closed
                })
            })
    }

    pub(crate) fn update_observation_control(&mut self, control: &MediaObservationControl) {
        if let Some(observation) = self.observations.get_mut(&control.observation) {
            observation.control = Some(control.clone());
            observation.completed = false;
            observation.closed = control.closed;
        }
    }

    pub(crate) fn observation_control_current(&self, control: &MediaObservationControl) -> bool {
        control.effect.is_some()
            && !control.closed
            && control.target.is_some()
            && self
                .observations
                .get(&control.observation)
                .is_some_and(|observation| {
                    !observation.closed && observation.control.as_ref() == Some(control)
                })
    }

    pub(crate) fn complete_observation_control(
        &mut self,
        control: &MediaObservationControl,
        succeeded: bool,
    ) -> bool {
        if !self.observation_control_current(control) {
            return false;
        }
        let Some(observation) = self.observations.get_mut(&control.observation) else {
            return false;
        };
        if observation.completed {
            return false;
        }
        observation.completed = true;
        observation.attached = succeeded;
        observation.closed = !succeeded;
        true
    }

    pub(crate) fn end_observation_control(&mut self, control: &MediaObservationControl) -> bool {
        if !self.observation_control_current(control) {
            return false;
        }
        let Some(observation) = self.observations.get_mut(&control.observation) else {
            return false;
        };
        observation.closed = true;
        true
    }

    pub(crate) fn release_observation_controls(
        &mut self,
        owner: TrackOwner,
    ) -> Vec<MediaObservationControl> {
        let mut controls = Vec::new();
        self.observations.retain(|id, _| {
            if id.owner != owner && id.request != owner {
                return true;
            }
            controls.push(MediaObservationControl {
                effect: None,
                observation: *id,
                target: None,
                closed: true,
            });
            false
        });
        controls
    }
}
