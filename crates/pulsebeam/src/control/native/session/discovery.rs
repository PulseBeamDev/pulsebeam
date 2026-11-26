use super::*;
use crate::entity::ParticipantId;

impl NativeSession {
    pub(super) fn unavailable(&mut self, key: Key) -> Result<(), OutputError> {
        self.put(key.clone(), absence(0, wire::AbsenceReason::Gone, None))?;
        if let Some(row) = self.rows.get_mut(&key)
            && !row.unavailable
        {
            row.unavailable = true;
            self.unavailable = self.unavailable.saturating_add(1);
        }
        Ok(())
    }

    pub(crate) fn observe_self(
        &mut self,
        participant: wire::Participant,
    ) -> Result<(), OutputError> {
        let key = Key::Participant((
            participant.room_id.clone(),
            participant.participant_id.clone(),
        ));
        self.put_participant(participant)?;
        self.rows
            .get_mut(&key)
            .ok_or(OutputError::InvalidObservation)?
            .self_context = true;
        Ok(())
    }

    pub(crate) fn observe_candidate(
        &mut self,
        source: super::super::ResolvedSource<'_>,
    ) -> Result<(), OutputError> {
        if !self.permissions.can_discover_source(&source) {
            return self.remove_track(source.track.id().as_str());
        }
        match track_descriptor(source.track) {
            Some(descriptor) => self.observe_track(descriptor, source.publisher),
            None => {
                let id = source.track.id().as_str();
                if self
                    .rows
                    .get(&Key::Publication(id.clone()))
                    .is_some_and(|row| {
                        matches!(
                            row.value.value,
                            Some(wire::confirmed_state::Value::Track(_))
                        )
                    })
                {
                    self.remove_track(id.clone())?;
                }
                self.unavailable(Key::Publication(id))
            }
        }
    }

    fn put_participant(&mut self, participant: wire::Participant) -> Result<(), OutputError> {
        validation::validate_participant(&participant)
            .map_err(|_| OutputError::InvalidObservation)?;
        let key = Key::Participant((
            participant.room_id.clone(),
            participant.participant_id.clone(),
        ));
        if self.rows.get(&key).is_some_and(|row| matches!(&row.value.value, Some(wire::confirmed_state::Value::Participant(current)) if current != &participant)) {
            return Err(OutputError::InvalidObservation);
        }
        self.put(
            key,
            confirmed(wire::confirmed_state::Value::Participant(participant)),
        )
    }

    pub(crate) fn observe_participant(
        &mut self,
        participant: wire::Participant,
    ) -> Result<(), OutputError> {
        validation::validate_participant(&participant)
            .map_err(|_| OutputError::InvalidObservation)?;
        let key = Key::Participant((
            participant.room_id.clone(),
            participant.participant_id.clone(),
        ));
        if self
            .permissions
            .can_discover_participant(Some(&participant.participant_id))
            || self
                .rows
                .get(&key)
                .is_some_and(|row| row.self_context || row.discovered_tracks != 0)
        {
            self.put_participant(participant)
        } else {
            self.remove_participant(participant.room_id, participant.participant_id)
        }
    }

    pub(crate) fn remove_participant(
        &mut self,
        room: String,
        id: String,
    ) -> Result<(), OutputError> {
        let key = Key::Participant((room, id));
        let Some(row) = self.rows.get(&key) else {
            return Ok(());
        };
        if row.self_context {
            return Ok(());
        }
        if row.discovered_tracks != 0 {
            self.discovery_reconciling = true;
            self.permissions_changed();
        }
        self.withdraw(&key)
    }

    pub(crate) fn observe_track(
        &mut self,
        track: wire::Track,
        publisher: &ParticipantId,
    ) -> Result<(), OutputError> {
        validation::validate_track(&track).map_err(|_| OutputError::InvalidObservation)?;
        if !self
            .permissions
            .can_discover_track(publisher.as_str(), &track)
        {
            return self.remove_track(track.publication_id);
        }
        let key = Key::Publication(track.publication_id.clone());
        let previous = self.rows.get(&key).and_then(|row| match &row.value.value {
            Some(wire::confirmed_state::Value::Track(current)) => Some(current),
            _ => None,
        });
        if previous.is_some_and(|current| current != &track) {
            return Err(OutputError::InvalidObservation);
        }
        let added = previous.is_none();
        let owner = Key::Participant((track.room_id.clone(), track.participant_id.clone()));
        self.put_participant(wire::Participant {
            participant_id: track.participant_id.clone(),
            room_id: track.room_id.clone(),
        })?;
        self.put(key, confirmed(wire::confirmed_state::Value::Track(track)))?;
        if added {
            let row = self
                .rows
                .get_mut(&owner)
                .ok_or(OutputError::InvalidObservation)?;
            row.discovered_tracks = row
                .discovered_tracks
                .checked_add(1)
                .ok_or(OutputError::Capacity)?;
        }
        Ok(())
    }

    pub(crate) fn remove_track(&mut self, id: String) -> Result<(), OutputError> {
        let key = Key::Publication(id);
        let Some(row) = self.rows.get(&key) else {
            return Ok(());
        };
        let owner = match &row.value.value {
            Some(wire::confirmed_state::Value::Track(track)) => Some(Key::Participant((
                track.room_id.clone(),
                track.participant_id.clone(),
            ))),
            _ => None,
        };
        self.withdraw(&key)?;
        if let Some(owner) = owner {
            let row = self
                .rows
                .get_mut(&owner)
                .ok_or(OutputError::InvalidObservation)?;
            row.discovered_tracks = row
                .discovered_tracks
                .checked_sub(1)
                .ok_or(OutputError::InvalidObservation)?;
            self.reconcile_participant(&owner)?;
            self.collect(&owner);
        }
        Ok(())
    }

    fn reconcile_participant(&mut self, key: &Key) -> Result<(), OutputError> {
        let Some(row) = self.rows.get(key) else {
            return Ok(());
        };
        if row.self_context || row.discovered_tracks != 0 {
            return Ok(());
        }
        let external = match &row.value.value {
            Some(wire::confirmed_state::Value::Participant(participant)) => {
                Some(participant.participant_id.as_str())
            }
            _ => None,
        };
        if !self.permissions.can_discover_participant(external) {
            self.withdraw(key)?;
        }
        Ok(())
    }

    fn withdraw(&mut self, key: &Key) -> Result<(), OutputError> {
        self.put(key.clone(), absence(0, wire::AbsenceReason::Removed, None))?;
        // In-flight records need withdrawal even before their admission acknowledgment.
        if self.rows.get(key).is_some_and(|row| !row.represented)
            && !self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.selected.iter().any(|selected| &selected.key == key))
        {
            if let Some(row) = self.rows.get_mut(key) {
                row.dirty = false;
                if let Some(ticket) = row.ticket.take() {
                    self.dirty.remove(&ticket);
                }
            }
            self.collect(key);
        }
        Ok(())
    }

    pub(super) fn reconcile_discovery<'a>(
        &mut self,
        key: &Key,
        source: &impl Fn(PublicationKey) -> Option<super::super::ResolvedSource<'a>>,
    ) -> Result<(), OutputError> {
        match key {
            Key::Publication(id) => {
                if let Some(source) =
                    id.parse::<crate::entity::PublicationId>()
                        .ok()
                        .and_then(|id| {
                            [
                                crate::entity::TrackKind::Audio,
                                crate::entity::TrackKind::Video,
                                crate::entity::TrackKind::Data,
                            ]
                            .into_iter()
                            .map(|kind| PublicationKey::new(kind, id))
                            .find_map(|target| {
                                source(target).filter(|source| source.track.id() == target)
                            })
                        })
                {
                    self.observe_candidate(source)
                } else {
                    self.remove_track(id.clone())
                }
            }
            Key::Participant(_) => self.reconcile_participant(key),
            _ => Ok(()),
        }
    }
}
