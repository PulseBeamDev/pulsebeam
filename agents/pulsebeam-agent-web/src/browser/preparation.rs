use super::*;
use agent_core::{
    SlotBinding,
    client_sdp::{Allocation, ApplyOutcome, Bindings, Error, Preparation},
};

#[derive(Default)]
struct MediaWork {
    targets: Vec<(MediaSlot, RtcRtpTransceiver)>,
    created: Vec<(MediaSlot, RtcRtpTransceiver)>,
    retirements: Vec<(String, RtcRtpTransceiver)>,
    slots: Vec<SlotBinding>,
    stopped: Vec<String>,
    allocating: bool,
    cleaning: bool,
    failure: Option<Error>,
}

fn event(
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

fn stop_transceiver(transceiver: &RtcRtpTransceiver) -> Result<(), Error> {
    Reflect::get(transceiver.as_ref(), &JsValue::from_str("stop"))
        .map_err(|_| Error::Observation)?
        .dyn_into::<Function>()
        .map_err(|_| Error::Observation)?
        .call0(transceiver.as_ref())
        .map_err(|_| Error::Observation)?;
    Ok(())
}

fn stop_media(
    connection: &RtcPeerConnection,
    mids: &[String],
) -> Result<Vec<(String, RtcRtpTransceiver)>, Error> {
    if mids.is_empty() {
        return Ok(Vec::new());
    }
    let mut physical = BTreeMap::new();
    for value in connection.get_transceivers().iter() {
        let transceiver = value
            .dyn_into::<RtcRtpTransceiver>()
            .map_err(|_| Error::Observation)?;
        if let Some(mid) = transceiver.mid()
            && physical.insert(mid, transceiver).is_some()
        {
            return Err(Error::Observation);
        }
    }
    let targets = mids
        .iter()
        .map(|mid| physical.remove_entry(mid).ok_or(Error::Observation))
        .collect::<Result<Vec<_>, _>>()?;
    for (_, transceiver) in &targets {
        stop_transceiver(transceiver)?;
    }
    Ok(targets)
}

fn create_media(
    peer: &mut Peer,
    allocations: &[Allocation],
    work: &mut MediaWork,
) -> Result<(), Error> {
    if allocations.iter().any(|request| {
        peer.transceivers
            .iter()
            .any(|(slot, _)| slot == &request.slot)
    }) {
        return Err(Error::Observation);
    }
    for allocation in allocations {
        let video = allocation.slot.kind() == agent_core::MediaKind::Video;
        let sending = allocation.slot.direction() == agent_core::MediaDirection::SendOnly;
        let transceiver = add_transceiver(
            &peer.connection,
            if video { "video" } else { "audio" },
            sending,
            video && sending,
        )
        .map_err(|_| Error::Observation)?;
        peer.transceivers
            .push((allocation.slot.clone(), transceiver.clone()));
        work.targets
            .push((allocation.slot.clone(), transceiver.clone()));
        work.created.push((allocation.slot.clone(), transceiver));
    }
    Ok(())
}

fn stop_created(work: &mut MediaWork) -> Result<(), Error> {
    work.cleaning = true;
    for (_, transceiver) in &work.created {
        if let Some(mid) = transceiver.mid() {
            work.stopped.push(mid);
        }
        stop_transceiver(transceiver)?;
    }
    Ok(())
}

async fn apply_descriptions(
    weak: &Weak<RuntimeInner>,
    generation: Generation,
    operation: OperationId,
    connection: &RtcPeerConnection,
    work: &mut MediaWork,
) -> Result<(), Error> {
    let failed = |stage: &str, error: JsValue| {
        if let Some(inner) = weak.upgrade() {
            inner.logger.warn(
                LOG_TARGET,
                &format!("RTP {operation:?} {stage} failed: {}", js_message(error)),
            );
        }
        Error::Observation
    };
    let offered = JsFuture::from(connection.create_offer())
        .await
        .map_err(|error| failed("createOffer", error))?;
    {
        let inner = weak.upgrade().ok_or(Error::CandidateState)?;
        if !inner.current_media(generation, operation)
            || inner.should_abort_media(generation, operation)
        {
            return Err(Error::CandidateState);
        }
    }
    // WebIDL dictionaries have no runtime constructor to check.
    let offer: RtcSessionDescriptionInit = offered.unchecked_into();
    if !work.cleaning {
        let inner = weak.upgrade().ok_or(Error::CandidateState)?;
        let mut peers = inner.peers.borrow_mut();
        let executor = peers
            .get_mut(&generation.get())
            .and_then(|peer| peer.sdp.as_mut())
            .ok_or(Error::CandidateState)?;
        let prepared =
            executor.prepare_local_offer(operation, &offer.get_sdp().ok_or(Error::Observation)?)?;
        offer.set_sdp(&prepared);
    }
    JsFuture::from(connection.set_local_description(&offer))
        .await
        .map_err(|error| failed("setLocalDescription", error))?;
    let answer = weak
        .upgrade()
        .ok_or(Error::CandidateState)?
        .media_answer(generation, operation, work)?;
    let description = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    description.set_sdp(&answer);
    JsFuture::from(connection.set_remote_description(&description))
        .await
        .map_err(|error| failed("setRemoteDescription", error))?;
    Ok(())
}

impl RuntimeInner {
    pub(super) fn prepare_media(
        self: &Rc<Self>,
        generation: Generation,
        operation: OperationId,
        requests: Vec<Preparation>,
    ) {
        let result = self
            .peers
            .borrow_mut()
            .get_mut(&generation.get())
            .and_then(|peer| peer.sdp.as_mut())
            .ok_or(Error::CandidateState)
            .and_then(|executor| executor.enqueue(operation, requests));
        match result {
            Ok(()) => self.start_media(generation),
            Err(error) => self.send_event(event(
                generation,
                operation,
                ApplyOutcome::NotApplied,
                Err(error),
            )),
        }
    }

    pub(super) fn allocate_media(
        self: &Rc<Self>,
        generation: Generation,
        operation: OperationId,
        allocations: Vec<Allocation>,
    ) {
        let result = self
            .peers
            .borrow_mut()
            .get_mut(&generation.get())
            .and_then(|peer| peer.sdp.as_mut())
            .ok_or(Error::CandidateState)
            .and_then(|executor| executor.enqueue_allocation(operation, allocations));
        match result {
            Ok(()) => self.start_media(generation),
            Err(error) => self.send_event(event(
                generation,
                operation,
                ApplyOutcome::NotApplied,
                Err(error),
            )),
        }
    }

    pub(super) fn retire_media(
        self: &Rc<Self>,
        generation: Generation,
        operation: OperationId,
        mids: Vec<String>,
    ) {
        let result = self
            .peers
            .borrow_mut()
            .get_mut(&generation.get())
            .and_then(|peer| peer.sdp.as_mut())
            .ok_or(Error::CandidateState)
            .and_then(|executor| executor.enqueue_retirement(operation, mids));
        match result {
            Ok(()) => self.start_media(generation),
            Err(error) => self.send_event(event(
                generation,
                operation,
                ApplyOutcome::NotApplied,
                Err(error),
            )),
        }
    }

    pub(super) fn cancel_media(self: &Rc<Self>, generation: Generation, operation: OperationId) {
        let queued = self
            .peers
            .borrow_mut()
            .get_mut(&generation.get())
            .and_then(|peer| peer.sdp.as_mut())
            .is_some_and(|executor| executor.cancel(operation));
        if queued {
            self.send_event(event(
                generation,
                operation,
                ApplyOutcome::NotApplied,
                Err(Error::CandidateState),
            ));
        }
    }

    fn current_media(&self, generation: Generation, operation: OperationId) -> bool {
        self.peers
            .borrow()
            .get(&generation.get())
            .and_then(|peer| peer.sdp.as_ref())
            .is_some_and(|executor| executor.operation() == Some(operation))
    }

    fn should_abort_media(&self, generation: Generation, operation: OperationId) -> bool {
        self.peers
            .borrow()
            .get(&generation.get())
            .and_then(|peer| peer.sdp.as_ref())
            .is_some_and(|executor| executor.should_abort(operation))
    }

    fn media_answer(
        &self,
        generation: Generation,
        operation: OperationId,
        work: &mut MediaWork,
    ) -> Result<String, Error> {
        let mut peers = self.peers.borrow_mut();
        let peer = peers
            .get_mut(&generation.get())
            .ok_or(Error::CandidateState)?;
        let actual = peer
            .connection
            .local_description()
            .ok_or(Error::Observation)?
            .sdp();
        let executor = peer.sdp.as_mut().ok_or(Error::CandidateState)?;
        let mut candidate = if work.cleaning {
            executor.prepare_abandoned_allocation(operation, &actual, &work.stopped)?
        } else if work.allocating {
            let mids = work
                .targets
                .iter()
                .map(|(slot, transceiver)| {
                    transceiver
                        .mid()
                        .map(|mid| (slot.clone(), mid))
                        .ok_or(Error::Observation)
                })
                .collect::<Result<Vec<_>, _>>()?;
            work.slots = agent_core::client_sdp::assigned_slot_bindings(&actual, mids)
                .map_err(|_| Error::Observation)?;
            for slot in &work.slots {
                if slot.direction == agent_core::MediaDirection::ReceiveOnly {
                    peer.receiver_mids
                        .insert(slot.media_index, slot.mid.clone());
                }
            }
            executor.prepare_allocated(operation, &actual, &work.slots)?
        } else {
            executor.prepare(operation, &actual)?
        };
        if !work.cleaning {
            let mids = candidate
                .bindings()
                .publications
                .iter()
                .map(|binding| binding.mid.clone())
                .collect::<Vec<_>>();
            for mid in mids {
                let transceiver = peer
                    .transceivers
                    .iter()
                    .map(|(_, transceiver)| transceiver)
                    .find(|transceiver| transceiver.mid().as_deref() == Some(mid.as_str()))
                    .ok_or(Error::Observation)?;
                let parameters = transceiver.sender().get_parameters();
                let encodings = Reflect::get(parameters.as_ref(), &JsValue::from_str("encodings"))
                    .map_err(|_| Error::Observation)?
                    .dyn_into::<Array>()
                    .map_err(|_| Error::Observation)?;
                let modes = encodings
                    .iter()
                    .map(|encoding| {
                        let mode = Reflect::get(&encoding, &JsValue::from_str("scalabilityMode"))
                            .map_err(|_| Error::Observation)?;
                        if mode.is_undefined() || mode.is_null() {
                            Ok(None)
                        } else {
                            mode.as_string().map(Some).ok_or(Error::Observation)
                        }
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                executor.prepare_sender_modes(operation, &mut candidate, &mid, &modes)?;
            }
        }
        Ok(candidate.answer().into())
    }

    fn finish_media(
        self: &Rc<Self>,
        generation: Generation,
        operation: OperationId,
        applied: bool,
        work: MediaWork,
    ) {
        if !self.current_media(generation, operation) {
            return;
        }
        let completion = {
            let mut peers = self.peers.borrow_mut();
            let Some(peer) = peers.get_mut(&generation.get()) else {
                return;
            };
            let Some(mut executor) = peer.sdp.take() else {
                return;
            };
            let finished = executor.finish(operation, applied);
            let outcome = match &finished {
                Ok(outcome) => *outcome,
                Err(_) if applied => ApplyOutcome::AppliedButCancelled,
                Err(_) => ApplyOutcome::NotApplied,
            };
            if applied {
                // Applying the answer can clear a stopped transceiver's MID.
                peer.transceivers.retain(|(_, transceiver)| {
                    !work.retirements.iter().any(|(mid, retired)| {
                        executor.context_mut().stopped(mid)
                            && js_sys::Object::is(transceiver.as_ref(), retired.as_ref())
                    })
                });
                peer.receiver_mids
                    .retain(|_, mid| !executor.context_mut().stopped(mid));
            }
            if work.cleaning {
                peer.transceivers.retain(|(_, transceiver)| {
                    !work.created.iter().any(|(_, created)| {
                        js_sys::Object::is(transceiver.as_ref(), created.as_ref())
                    })
                });
                peer.receiver_mids
                    .retain(|_, mid| !work.stopped.contains(mid));
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
            let preparation = finished.and_then(|outcome| {
                if let Some(error) = work.failure {
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
            peer.sdp = Some(executor);
            HostEvent::Rtc(RtcEvent::MediaPrepared {
                generation,
                operation,
                slots,
                outcome,
                preparation,
            })
        };
        self.send_event(completion);
        self.start_media(generation);
    }

    fn start_media(self: &Rc<Self>, generation: Generation) {
        let next = {
            let mut peers = self.peers.borrow_mut();
            let Some(peer) = peers.get_mut(&generation.get()) else {
                return;
            };
            let Some(operation) = peer
                .sdp
                .as_mut()
                .and_then(agent_core::client_sdp::Executor::start)
            else {
                return;
            };
            let local_slots = (|| {
                let executor = peer.sdp.as_ref().ok_or(Error::CandidateState)?;
                let mids = executor.retirement_targets(operation)?;
                let allocations = executor.allocation_targets(operation)?;
                Ok::<_, Error>(
                    allocations
                        .iter()
                        .map(|allocation| &allocation.slot)
                        .chain(peer.transceivers.iter().filter_map(|(slot, transceiver)| {
                            transceiver
                                .mid()
                                .filter(|mid| mids.contains(mid))
                                .map(|_| slot)
                        }))
                        .filter_map(|slot| match slot {
                            MediaSlot::LocalAudio(name) | MediaSlot::LocalVideo(name) => {
                                Some(name.clone())
                            }
                            _ => None,
                        })
                        .collect::<std::collections::BTreeSet<_>>(),
                )
            })();
            (operation, peer.connection.clone(), local_slots)
        };
        let (operation, connection, local_slots) = next;
        let weak = Rc::downgrade(self);
        spawn_local(async move {
            let mut work = MediaWork::default();
            let Some(inner) = weak.upgrade() else { return };
            // Hold source-slot permits until stopped senders leave the peer map.
            let mut operations = Vec::new();
            let mut permits = Vec::new();
            let initialized = async {
                operations = local_slots?
                    .into_iter()
                    .map(|slot| inner.local_operations(&slot))
                    .collect();
                for operations in &operations {
                    permits.push(operations.gate.enter().await);
                }
                if inner.is_closed() {
                    return Err(Error::CandidateState);
                }
                let mut peers = inner.peers.borrow_mut();
                let peer = peers
                    .get_mut(&generation.get())
                    .ok_or(Error::CandidateState)?;
                let executor = peer.sdp.as_ref().ok_or(Error::CandidateState)?;
                let mids = executor.retirement_targets(operation)?.to_vec();
                let allocations = executor.allocation_targets(operation)?.to_vec();
                work.allocating = !allocations.is_empty();
                work.retirements = stop_media(&peer.connection, &mids)?;
                create_media(peer, &allocations, &mut work)
            }
            .await;
            if let Err(error) = initialized {
                work.failure = Some(error);
                if work.created.is_empty() || stop_created(&mut work).is_err() {
                    inner.finish_media(generation, operation, false, work);
                    return;
                }
            }
            drop(inner);
            if !work.cleaning {
                for (slot, _) in &work.created {
                    if let MediaSlot::LocalAudio(name) | MediaSlot::LocalVideo(name) = slot {
                        let Some(inner) = weak.upgrade() else {
                            return;
                        };
                        let synchronized = inner.sync_local_slot(name, None).await;
                        if synchronized.is_err() {
                            work.failure = Some(Error::Observation);
                            break;
                        }
                    }
                }
                if work.failure.is_some() && stop_created(&mut work).is_err() {
                    if let Some(inner) = weak.upgrade() {
                        inner.finish_media(generation, operation, false, work);
                    }
                    return;
                }
            }
            let mut applied = false;
            for _ in 0..2 {
                match apply_descriptions(&weak, generation, operation, &connection, &mut work).await
                {
                    Ok(()) => {
                        applied = true;
                        break;
                    }
                    Err(error) => {
                        work.failure.get_or_insert(error);
                        if work.created.is_empty()
                            || work.cleaning
                            || stop_created(&mut work).is_err()
                        {
                            break;
                        }
                    }
                }
            }
            if let Some(inner) = weak.upgrade() {
                inner.finish_media(generation, operation, applied, work);
            }
            drop(permits);
        });
    }
}
