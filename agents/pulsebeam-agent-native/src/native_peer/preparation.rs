use super::*;
use agent_core::{
    OperationId, SlotBinding,
    client_sdp::{Allocation, ApplyOutcome, Bindings, Error, Executor, Preparation},
};

#[derive(Default)]
pub(super) struct MediaWork {
    allocations: Vec<Allocation>,
    created: BTreeMap<MediaSlot, rtc::SessionTransceiverId>,
    slots: Vec<SlotBinding>,
    stopped: Vec<String>,
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
                .created
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
        self.sdp
            .as_mut()
            .is_some_and(|executor| executor.cancel(operation))
            .then(|| {
                media_event(
                    self.generation,
                    operation,
                    ApplyOutcome::NotApplied,
                    Err(Error::CandidateState),
                )
            })
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
            ))
        })();
        let (mids, allocations) = match targets {
            Ok(targets) => targets,
            Err(error) => return self.finish_media(operation, false, Some(error), events),
        };
        self.media_work = Some(MediaWork {
            allocations,
            retiring: !mids.is_empty(),
            ..Default::default()
        });
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
                created = self.media_work.as_ref().map_or(0, |work| work.created.len()),
                "native RTP preparation failed");
        }
        let Some(work) = self
            .media_work
            .as_mut()
            .filter(|work| !work.created.is_empty() && !work.cleaning)
        else {
            return self.finish_media(operation, false, Some(failure), events);
        };
        work.cleaning = true;
        work.failure = Some(failure);
        for transceiver in work.created.values() {
            if let Ok(Some(mid)) = self.session.transceiver_mid(*transceiver) {
                work.stopped.push(mid);
            }
            if self.session.stop_standard(*transceiver).is_err() {
                return self.finish_media(operation, false, Some(Error::Observation), events);
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
            Stage::MediaSettingRemote(_) => self.finish_media(operation, true, None, events)?,
            _ => return Err(error("native media stage is absent")),
        }
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
                executor.prepare_sender_modes(operation, &mut candidate, &mid, &modes)?;
            }
        }
        Ok(candidate.answer().to_owned())
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
        if work.cleaning {
            for (slot, transceiver) in &work.created {
                if let Err(error) = self.drop_media_mapping(slot, *transceiver) {
                    cleaned = Err(error);
                }
            }
        }
        let observed = executor
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
