use super::*;
use agent_core::{
    OperationId, SlotBinding,
    client_sdp::{Allocation, ApplyOutcome, Bindings, Error, Executor, Preparation},
};

#[derive(Default)]
pub(super) struct MediaWork {
    allocations: Vec<Allocation>,
    owned: BTreeMap<MediaSlot, rtc::SessionTransceiverId>,
    slots: Vec<SlotBinding>,
    stopped: Vec<String>,
    receiving: bool,
    observed: Option<Box<agent_core::client_sdp::Binding>>,
    retiring: bool,
    cleaning: bool,
    failure: Option<Error>,
}

pub(crate) fn media_event(
    generation: Generation,
    operation: OperationId,
    outcome: ApplyOutcome,
    preparation: Result<Bindings, Error>,
) -> HostEvent {
    HostEvent::Rtc(RtcEvent::MediaPrepared {
        generation,
        operation,
        slots: Vec::new(),
        outcome,
        preparation,
    })
}

impl<R: rtc::SessionRuntime> NativePeer<R> {
    pub fn prepare_media(
        &mut self,
        operation: OperationId,
        requests: Vec<Preparation>,
    ) -> Option<HostEvent> {
        self.sdp
            .as_mut()
            .ok_or(Error::CandidateState)
            .and_then(|executor| executor.enqueue(operation, requests))
            .err()
            .map(|error| {
                media_event(
                    self.generation,
                    operation,
                    ApplyOutcome::NotApplied,
                    Err(error),
                )
            })
    }

    pub fn allocate_media(
        &mut self,
        operation: OperationId,
        allocations: Vec<Allocation>,
        config: &MediaConfig,
    ) -> Option<HostEvent> {
        self.media_config = config.clone();
        self.sdp
            .as_mut()
            .ok_or(Error::CandidateState)
            .and_then(|executor| executor.enqueue_allocation(operation, allocations))
            .err()
            .map(|error| {
                media_event(
                    self.generation,
                    operation,
                    ApplyOutcome::NotApplied,
                    Err(error),
                )
            })
    }

    pub fn offer_receive_media(
        &mut self,
        operation: OperationId,
        target: agent_core::ReceiveTarget,
    ) -> Option<HostEvent> {
        self.sdp
            .as_mut()
            .ok_or(Error::CandidateState)
            .and_then(|executor| match target {
                agent_core::ReceiveTarget::Existing(slot) => {
                    executor.enqueue_reception(operation, slot)
                }
                agent_core::ReceiveTarget::Allocate(allocation) => {
                    executor.enqueue_allocation(operation, vec![allocation])
                }
            })
            .err()
            .map(|error| {
                media_event(
                    self.generation,
                    operation,
                    ApplyOutcome::NotApplied,
                    Err(error),
                )
            })
    }

    pub fn apply_receive_plan(
        &mut self,
        operation: OperationId,
        slot: SlotBinding,
        parameters: agent_core::RtpParameters,
    ) -> Option<HostEvent> {
        if !matches!(self.stage, Stage::MediaAwaitingPlan)
            || self.sdp.as_ref().and_then(Executor::operation) != Some(operation)
            || !self.media_work.as_ref().is_some_and(|work| {
                work.receiving && work.slots.as_slice() == std::slice::from_ref(&slot)
            })
        {
            return None;
        }
        let applied = (|| {
            let descriptions = self
                .session
                .descriptions(self.peer)
                .map_err(|_| Error::Observation)?;
            let offer = &descriptions.local().ok_or(Error::Observation)?.sdp;
            let mut candidate = self
                .sdp
                .as_mut()
                .ok_or(Error::CandidateState)?
                .prepare_received(operation, offer, &slot, &parameters)?;
            self.capture_sender_modes(operation, &mut candidate)?;
            let native = self
                .session
                .set_remote_description(
                    self.peer,
                    rtc::SessionDescription {
                        kind: rtc::SessionDescriptionType::Answer,
                        sdp: candidate.answer().to_owned(),
                    },
                )
                .map_err(|_| Error::Observation)?;
            self.stage = Stage::MediaSettingRemote(native);
            Ok::<_, Error>(())
        })();
        applied
            .err()
            .and_then(|error| self.repair_event(operation, error))
    }

    fn repair_event(&mut self, operation: OperationId, failure: Error) -> Option<HostEvent> {
        let mut events = Vec::new();
        if let Err(error) = self.repair_media(operation, failure, &mut events) {
            tracing::warn!(
                ?operation,
                ?error,
                "native receive repair could not complete"
            );
            return Some(media_event(
                self.generation,
                operation,
                ApplyOutcome::NotApplied,
                Err(Error::Observation),
            ));
        }
        events.pop()
    }

    pub fn retire_media(&mut self, operation: OperationId, mids: Vec<String>) -> Option<HostEvent> {
        self.sdp
            .as_mut()
            .ok_or(Error::CandidateState)
            .and_then(|executor| executor.enqueue_retirement(operation, mids))
            .err()
            .map(|error| {
                media_event(
                    self.generation,
                    operation,
                    ApplyOutcome::NotApplied,
                    Err(error),
                )
            })
    }

    pub(super) fn stop_media(&mut self, mids: &[String]) -> Result<(), Error> {
        if mids.is_empty() {
            return Ok(());
        }
        let mut physical = BTreeMap::new();
        for transceiver in self
            .session
            .transceivers(self.peer)
            .map_err(|_| Error::Observation)?
        {
            if let Some(mid) = self
                .session
                .transceiver_mid(transceiver)
                .map_err(|_| Error::Observation)?
                && physical.insert(mid, transceiver).is_some()
            {
                return Err(Error::Observation);
            }
        }
        let targets = mids
            .iter()
            .map(|mid| physical.get(mid).copied().ok_or(Error::Observation))
            .collect::<Result<Vec<_>, _>>()?;
        for transceiver in targets {
            tracing::debug!(
                generation = ?self.generation,
                ?transceiver,
                mid = ?self.session.transceiver_mid(transceiver),
                "native media transceiver retirement"
            );
            self.session
                .stop_standard(transceiver)
                .map_err(|_| Error::Observation)?;
        }
        Ok(())
    }

    fn create_media(&mut self) -> Result<(), Error> {
        let allocations = self
            .media_work
            .as_ref()
            .ok_or(Error::CandidateState)?
            .allocations
            .clone();
        if allocations
            .iter()
            .any(|request| self.transceivers.contains_key(&request.slot))
        {
            return Err(Error::Observation);
        }
        for allocation in allocations {
            let slot = allocation.slot;
            let receiving = slot.direction() == agent_core::MediaDirection::ReceiveOnly;
            let kind = if slot.kind() == agent_core::MediaKind::Audio {
                rtc::MediaType::Audio
            } else {
                rtc::MediaType::Video
            };
            let profile = match &slot {
                MediaSlot::LocalVideo(name) => self
                    .media_config
                    .video_publications
                    .iter()
                    .find(|profile| &profile.slot == name)
                    .map(|profile| profile.source.clone())
                    .unwrap_or_default(),
                _ => VideoSourceConfig::default(),
            };
            let transceiver = if receiving {
                let mut init = rtc::RtpTransceiverInit::default();
                init.set_direction(rtc::RtpTransceiverDirectionValue::ReceiveOnly);
                self.session
                    .add_transceiver(self.peer, kind, &init)
                    .map_err(|_| Error::Observation)?
            } else {
                let (MediaSlot::LocalAudio(name) | MediaSlot::LocalVideo(name)) = &slot else {
                    return Err(Error::Identifier);
                };
                let source_kind = if kind == rtc::MediaType::Audio {
                    SourceKind::Audio(self.media_config.audio_input)
                } else {
                    SourceKind::Video(self.media_config.video_input)
                };
                let source =
                    NativeSource::create(&mut self.session, self.peer, name, source_kind, &profile)
                        .map_err(|_| Error::Observation)?;
                let transceiver = source.transceiver;
                self.sources.insert(slot.clone(), source);
                transceiver
            };
            self.transceivers.insert(slot.clone(), transceiver);
            self.media_work
                .as_mut()
                .ok_or(Error::CandidateState)?
                .owned
                .insert(slot, transceiver);
            codecs_and_extensions(
                &mut self.session,
                transceiver,
                receiving,
                profile.dependency_descriptor,
            )
            .map_err(|_| Error::Observation)?;
        }
        Ok(())
    }

    fn drop_media_mapping(
        &mut self,
        slot: &MediaSlot,
        transceiver: rtc::SessionTransceiverId,
    ) -> Result<(), Error> {
        if self.transceivers.get(slot) != Some(&transceiver) {
            return Ok(());
        }
        if !self
            .session
            .transceiver_stopped(transceiver)
            .map_err(|_| Error::Observation)?
        {
            return Err(Error::Observation);
        }
        if let Some(source) = self
            .sources
            .get_mut(slot)
            .filter(|source| source.transceiver == transceiver)
        {
            source
                .close(&mut self.session)
                .map_err(|_| Error::Observation)?;
            self.sources.remove(slot);
        }
        if let Some(receiver) = self.receivers.get_mut(slot) {
            receiver
                .detach(&mut self.session)
                .map_err(|_| Error::Observation)?;
            self.receivers.remove(slot);
        }
        self.transceivers.remove(slot);
        self.bindings.remove(slot);
        Ok(())
    }

    fn prune_stopped_media(
        &mut self,
        context: &agent_core::client_sdp::Context,
    ) -> Result<(), Error> {
        let mut stopped = Vec::new();
        for (slot, binding) in &self.bindings {
            if !context.stopped(&binding.mid) {
                continue;
            }
            let transceiver = *self.transceivers.get(slot).ok_or(Error::Observation)?;
            // Rejected-section removal can clear MID before the stopped object is pruned.
            if self
                .session
                .transceiver_mid(transceiver)
                .map_err(|_| Error::Observation)?
                .is_none_or(|mid| mid == binding.mid)
            {
                stopped.push((slot.clone(), transceiver));
            }
        }
        for (slot, transceiver) in stopped {
            self.drop_media_mapping(&slot, transceiver)?;
        }
        Ok(())
    }

    pub fn cancel_media(&mut self, operation: OperationId) -> Option<HostEvent> {
        let queued = self
            .sdp
            .as_mut()
            .is_some_and(|executor| executor.cancel(operation));
        if queued {
            return Some(media_event(
                self.generation,
                operation,
                ApplyOutcome::NotApplied,
                Err(Error::CandidateState),
            ));
        }
        if matches!(self.stage, Stage::MediaAwaitingPlan)
            && self
                .sdp
                .as_ref()
                .is_some_and(|executor| executor.cancelled(operation))
        {
            return self.repair_event(operation, Error::CandidateState);
        }
        None
    }

    pub(super) fn start_media(
        &mut self,
        events: &mut Vec<HostEvent>,
    ) -> Result<(), rtc::PeerError> {
        let Some(operation) = self.sdp.as_mut().and_then(Executor::start) else {
            return Ok(());
        };
        let targets = (|| {
            let executor = self.sdp.as_ref().ok_or(Error::CandidateState)?;
            Ok::<_, Error>((
                executor.retirement_targets(operation)?.to_vec(),
                executor.allocation_targets(operation)?.to_vec(),
                executor.reception_target(operation)?.cloned(),
            ))
        })();
        let (mids, allocations, existing) = match targets {
            Ok(targets) => targets,
            Err(error) => return self.finish_media(operation, false, Some(error), events),
        };
        let receiving = existing.is_some()
            || (!allocations.is_empty()
                && allocations.iter().all(|allocation| {
                    allocation.slot.direction() == agent_core::MediaDirection::ReceiveOnly
                }));
        if receiving && allocations.len() > 1 {
            return self.finish_media(operation, false, Some(Error::Identifier), events);
        }
        self.media_work = Some(MediaWork {
            allocations,
            receiving,
            retiring: !mids.is_empty(),
            ..Default::default()
        });
        if let Some(slot) = existing {
            let target = self
                .transceivers
                .get(&slot.slot)
                .copied()
                .filter(|_| self.bindings.get(&slot.slot) == Some(&slot));
            let Some(transceiver) = target else {
                return self.finish_media(operation, false, Some(Error::Observation), events);
            };
            let work = self
                .media_work
                .as_mut()
                .ok_or_else(|| error("native media work is absent"))?;
            work.owned.insert(slot.slot.clone(), transceiver);
            work.slots.push(slot);
        }
        if let Err(error) = self.stop_media(&mids).and_then(|()| self.create_media()) {
            return self.repair_media(operation, error, events);
        }
        match self.session.create_offer(self.peer, false) {
            Ok(native) => self.stage = Stage::MediaCreatingOffer(native),
            Err(_) => self.repair_media(operation, Error::Observation, events)?,
        }
        Ok(())
    }

    fn repair_media(
        &mut self,
        operation: OperationId,
        failure: Error,
        events: &mut Vec<HostEvent>,
    ) -> Result<(), rtc::PeerError> {
        if !matches!(failure, Error::CandidateState) {
            tracing::warn!(?operation, ?failure, stage = ?self.stage,
                allocations = ?self.media_work.as_ref().map(|work| &work.allocations),
                owned = self.media_work.as_ref().map_or(0, |work| work.owned.len()),
                "native RTP preparation failed");
        }
        let Some(work) = self
            .media_work
            .as_mut()
            .filter(|work| !work.owned.is_empty() && !work.cleaning)
        else {
            return self.finish_media(operation, false, Some(failure), events);
        };
        work.cleaning = true;
        work.failure = Some(failure);
        for transceiver in work.owned.values() {
            let mid = self.session.transceiver_mid(*transceiver).ok().flatten();
            if self.session.stop_standard(*transceiver).is_err() {
                return self.finish_media(operation, false, Some(Error::Observation), events);
            }
            if let Some(mid) = mid {
                work.stopped.push(mid);
            }
        }
        match self.session.set_local_description_auto(self.peer) {
            Ok(native) => {
                self.stage = Stage::MediaSettingLocal(native);
                Ok(())
            }
            Err(_) => self.finish_media(operation, false, Some(Error::Observation), events),
        }
    }

    pub(super) fn media_completed(
        &mut self,
        result: Result<Option<rtc::SessionDescription>, rtc::PeerError>,
        events: &mut Vec<HostEvent>,
        observations: &mut Vec<Observation>,
    ) -> Result<(), rtc::PeerError> {
        let operation = self
            .sdp
            .as_ref()
            .and_then(Executor::operation)
            .ok_or_else(|| error("native media operation is absent"))?;
        if self
            .sdp
            .as_ref()
            .is_some_and(|executor| executor.should_abort(operation))
            && !matches!(self.stage, Stage::MediaSettingRemote(_))
            && !self.media_work.as_ref().is_some_and(|work| work.cleaning)
        {
            return self.repair_media(operation, Error::CandidateState, events);
        }
        let description = match result {
            Ok(description) => description,
            Err(error) => {
                tracing::warn!(?operation, ?error, stage = ?self.stage, "native RTP operation failed");
                return self.repair_media(operation, Error::Observation, events);
            }
        };
        match self.stage {
            Stage::MediaCreatingOffer(_) => {
                let prepared = (|| {
                    let description = description.ok_or(Error::Observation)?;
                    if !matches!(description.kind, rtc::SessionDescriptionType::Offer) {
                        return Err(Error::Observation);
                    }
                    self.sdp
                        .as_mut()
                        .ok_or(Error::CandidateState)?
                        .prepare_local_offer(operation, &description.sdp)
                })();
                let sdp = match prepared {
                    Ok(sdp) => sdp,
                    Err(error) => return self.repair_media(operation, error, events),
                };
                match self.session.set_local_description(
                    self.peer,
                    rtc::SessionDescription {
                        kind: rtc::SessionDescriptionType::Offer,
                        sdp,
                    },
                ) {
                    Ok(native) => self.stage = Stage::MediaSettingLocal(native),
                    Err(_) => self.repair_media(operation, Error::Observation, events)?,
                }
            }
            Stage::MediaSettingLocal(_) => {
                if self
                    .media_work
                    .as_ref()
                    .is_some_and(|work| work.receiving && !work.cleaning)
                {
                    match self.receive_offer(operation) {
                        Ok(event) => {
                            self.stage = Stage::MediaAwaitingPlan;
                            events.push(event);
                        }
                        Err(error) => self.repair_media(operation, error, events)?,
                    }
                    return Ok(());
                }
                let answer = self.media_answer(operation);
                let answer = match answer {
                    Ok(answer) => answer,
                    Err(error) => return self.repair_media(operation, error, events),
                };
                match self.session.set_remote_description(
                    self.peer,
                    rtc::SessionDescription {
                        kind: rtc::SessionDescriptionType::Answer,
                        sdp: answer,
                    },
                ) {
                    Ok(native) => self.stage = Stage::MediaSettingRemote(native),
                    Err(_) => self.repair_media(operation, Error::Observation, events)?,
                }
            }
            Stage::MediaSettingRemote(_) => {
                let failure = if self
                    .media_work
                    .as_ref()
                    .is_some_and(|work| work.receiving && !work.cleaning)
                {
                    self.observe_receive(observations).err()
                } else {
                    None
                };
                self.finish_media(operation, true, failure, events)?;
            }
            _ => return Err(error("native media stage is absent")),
        }
        Ok(())
    }

    fn receive_offer(&mut self, operation: OperationId) -> Result<HostEvent, Error> {
        let descriptions = self
            .session
            .descriptions(self.peer)
            .map_err(|_| Error::Observation)?;
        let offer = &descriptions.local().ok_or(Error::Observation)?.sdp;
        let work = self.media_work.as_mut().ok_or(Error::CandidateState)?;
        let mids = work
            .owned
            .iter()
            .map(|(slot, transceiver)| {
                let mid = self
                    .session
                    .transceiver_mid(*transceiver)
                    .map_err(|_| Error::Observation)?
                    .ok_or(Error::Observation)?;
                Ok((slot.clone(), mid))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let slots = agent_core::client_sdp::assigned_slot_bindings(offer, mids)?;
        let [slot] = slots.as_slice() else {
            return Err(Error::Identifier);
        };
        let offered = self
            .sdp
            .as_mut()
            .ok_or(Error::CandidateState)?
            .receive_offer(operation, offer, slot)?;
        let slot = slot.clone();
        self.bindings.insert(slot.slot.clone(), slot.clone());
        work.slots = slots;
        Ok(HostEvent::Rtc(RtcEvent::MediaOffered {
            generation: self.generation,
            operation,
            slot,
            offer: offered,
        }))
    }

    fn observe_receive(&mut self, observations: &mut Vec<Observation>) -> Result<(), Error> {
        let work = self.media_work.as_ref().ok_or(Error::CandidateState)?;
        let [slot] = work.slots.as_slice() else {
            return Err(Error::Identifier);
        };
        let slot = slot.clone();
        let descriptions = self
            .session
            .descriptions(self.peer)
            .map_err(|_| Error::Observation)?;
        let answer = &descriptions.remote().ok_or(Error::Observation)?.sdp;
        let actual = self
            .sdp
            .as_mut()
            .ok_or(Error::CandidateState)?
            .context_mut()
            .observe_receive_answer(answer, &slot.mid)?;
        let transceiver = *self
            .transceivers
            .get(&slot.slot)
            .ok_or(Error::Observation)?;
        if self
            .session
            .transceiver_stopped(transceiver)
            .map_err(|_| Error::Observation)?
            || self
                .session
                .transceiver_mid(transceiver)
                .map_err(|_| Error::Observation)?
                .as_deref()
                != Some(slot.mid.as_str())
        {
            return Err(Error::Observation);
        }
        let receiver = self
            .session
            .receiver(transceiver)
            .map_err(|_| Error::Observation)?;
        let parameters = self
            .session
            .get_receiver_parameters(receiver)
            .map_err(|_| Error::Observation)?;
        let codecs = parameters.codecs();
        if !codecs.iter().any(|codec| {
            codec.payload_type() == i32::from(actual.codec.payload_type)
                && codec.mime_type_bytes().is_some_and(|mime| {
                    mime.eq_ignore_ascii_case(
                        format!(
                            "{}/{}",
                            if actual.kind == agent_core::client_sdp::Kind::Audio {
                                "audio"
                            } else {
                                "video"
                            },
                            actual.codec.parameters.name
                        )
                        .as_bytes(),
                    )
                })
        }) {
            return Err(Error::Observation);
        }
        if let Some(repair) = &actual.codec.repair
            && !codecs.iter().any(|codec| {
                codec.payload_type() == i32::from(repair.payload_type)
                    && codec.mime_type_bytes().is_some_and(|mime| {
                        mime.eq_ignore_ascii_case(
                            format!(
                                "{}/rtx",
                                if actual.kind == agent_core::client_sdp::Kind::Audio {
                                    "audio"
                                } else {
                                    "video"
                                }
                            )
                            .as_bytes(),
                        )
                    })
            })
        {
            return Err(Error::Observation);
        }
        let extensions = parameters.header_extensions();
        if actual.extensions.iter().any(|expected| {
            !(0..extensions.len()).any(|index| {
                extensions.get(index).is_some_and(|extension| {
                    extension.id().to_int() == i32::from(expected.id)
                        && extension.uri_bytes() == expected.uri.as_bytes()
                })
            })
        }) {
            return Err(Error::Observation);
        }
        self.refresh_receiver(transceiver, receiver, observations)
            .map_err(|_| Error::Observation)?;
        self.media_work
            .as_mut()
            .ok_or(Error::CandidateState)?
            .observed = Some(Box::new(actual));
        Ok(())
    }

    fn media_answer(&mut self, operation: OperationId) -> Result<String, Error> {
        let descriptions = self
            .session
            .descriptions(self.peer)
            .map_err(|_| Error::Observation)?;
        let offer = &descriptions.local().ok_or(Error::Observation)?.sdp;
        let work = self.media_work.as_mut().ok_or(Error::CandidateState)?;
        let executor = self.sdp.as_mut().ok_or(Error::CandidateState)?;
        let mut candidate = if work.cleaning {
            executor.prepare_abandoned_allocation(operation, offer, &work.stopped)?
        } else if !work.allocations.is_empty() {
            let mids = work
                .allocations
                .iter()
                .map(|allocation| {
                    let slot = &allocation.slot;
                    let transceiver = *self.transceivers.get(slot).ok_or(Error::Observation)?;
                    let mid = self
                        .session
                        .transceiver_mid(transceiver)
                        .map_err(|_| Error::Observation)?
                        .ok_or(Error::Observation)?;
                    Ok((slot.clone(), mid))
                })
                .collect::<Result<Vec<_>, Error>>()?;
            work.slots = agent_core::client_sdp::assigned_slot_bindings(offer, mids)?;
            for binding in &work.slots {
                self.bindings.insert(binding.slot.clone(), binding.clone());
            }
            executor.prepare_allocated(operation, offer, &work.slots)?
        } else {
            executor.prepare(operation, offer)?
        };
        if !work.cleaning {
            self.capture_sender_modes(operation, &mut candidate)?;
        }
        Ok(candidate.answer().to_owned())
    }

    fn capture_sender_modes(
        &mut self,
        operation: OperationId,
        candidate: &mut agent_core::client_sdp::Candidate,
    ) -> Result<(), Error> {
        let mids = candidate
            .bindings()
            .publications
            .iter()
            .map(|binding| binding.mid.clone())
            .collect::<Vec<_>>();
        for mid in mids {
            let transceiver = self
                .transceivers
                .iter()
                .find_map(|(slot, transceiver)| {
                    self.bindings
                        .get(slot)
                        .filter(|binding| binding.mid == mid)
                        .map(|_| *transceiver)
                })
                .ok_or(Error::Observation)?;
            let sender = self
                .session
                .sender(transceiver)
                .map_err(|_| Error::Observation)?;
            let modes = self
                .session
                .get_sender_parameters(sender)
                .map_err(|_| Error::Observation)?
                .encodings()
                .into_iter()
                .map(|encoding| {
                    encoding
                        .scalability_mode_bytes()
                        .map(String::from_utf8)
                        .transpose()
                        .map_err(|_| Error::Observation)
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.sdp
                .as_mut()
                .ok_or(Error::CandidateState)?
                .prepare_sender_modes(operation, candidate, &mid, &modes)?;
        }

        Ok(())
    }

    fn finish_media(
        &mut self,
        operation: OperationId,
        applied: bool,
        failure: Option<Error>,
        events: &mut Vec<HostEvent>,
    ) -> Result<(), rtc::PeerError> {
        let mut executor = self
            .sdp
            .take()
            .ok_or_else(|| error("native media context is absent"))?;
        let work = self.media_work.take().unwrap_or_default();
        let finished = executor.finish(operation, applied);
        let outcome = match &finished {
            Ok(outcome) => *outcome,
            Err(_) if applied => ApplyOutcome::AppliedButCancelled,
            Err(_) => ApplyOutcome::NotApplied,
        };
        let mut cleaned = if applied {
            self.prune_stopped_media(executor.context_mut())
        } else {
            Ok(())
        };
        if work.cleaning && applied {
            for (slot, transceiver) in &work.owned {
                if let Err(error) = self.drop_media_mapping(slot, *transceiver) {
                    cleaned = Err(error);
                }
            }
        }
        let mut observed = executor
            .context_mut()
            .bindings()
            .filter(|bindings| {
                bindings
                    .publications
                    .iter()
                    .all(agent_core::client_sdp::Binding::complete)
            })
            .cloned()
            .ok_or(Error::CandidateState);
        if let (Ok(bindings), Some(actual)) = (&mut observed, work.observed) {
            bindings
                .receptions
                .retain(|binding| binding.mid != actual.mid);
            bindings.receptions.push(*actual);
        }
        if let Err(error) = &observed {
            tracing::warn!(
                ?operation,
                ?error,
                applied,
                retiring = work.retiring,
                cleaning = work.cleaning,
                "native applied RTP binding incomplete"
            );
        }
        let preparation = finished.and_then(|outcome| {
            cleaned?;
            if work.receiving
                && work.cleaning
                && applied
                && outcome == ApplyOutcome::AppliedButCancelled
            {
                return Ok(executor.context_mut().applied_bindings().clone());
            }
            if let Some(error) = failure.or(work.failure) {
                return Err(error);
            }
            if outcome != ApplyOutcome::Applied {
                return Err(Error::CandidateState);
            }
            observed
        });
        let slots = if applied && !work.cleaning {
            work.slots
        } else {
            Vec::new()
        };
        self.sdp = Some(executor);
        self.stage = Stage::Ready;
        events.push(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation: self.generation,
            operation,
            slots,
            outcome,
            preparation,
        }));
        Ok(())
    }
}
