use super::*;

#[cfg(test)]
mod tests;

pub(super) enum Stage {
    Offering,
    Offered,
    Applying { reference: u64 },
    Cancelling,
}

pub(super) fn offer(
    session: &mut Session,
    resource: &Resource,
    slot: &MediaSlot,
    source: &str,
) -> Option<state::RtpReceiveOffer> {
    let lease = session.media_leases.get_mut(slot)?;
    if lease.resource != *resource || lease.source.as_deref() != Some(source) {
        lease.retired = true;
    }
    (!lease.retired)
        .then(|| lease.receive_offer.clone())
        .flatten()
}

pub(super) fn installed(session: &Session, slot: &MediaSlot, live: &state::LiveResource) -> bool {
    session
        .media_leases
        .get(slot)
        .is_some_and(|lease| !lease.retired && lease.receive_offer.is_some())
        && session
            .media_preparation
            .as_ref()
            .and_then(|cache| cache.as_ref().ok())
            .and_then(|cache| cache.get(slot))
            .and_then(|prepared| prepared.as_ref().ok())
            .is_some_and(|prepared| {
                state::validation::bindings_equivalent(
                    Some(&prepared.binding),
                    live.binding.as_ref(),
                )
            })
}

pub(super) fn project(session: &Session, snapshot: &mut Snapshot, slot: &MediaSlot) -> bool {
    let Some(coordinate) = session.coordinates.get(slot).and_then(Option::as_ref) else {
        return false;
    };
    let source = session
        .state
        .eligible(&Resource::Reception(slot.clone()))
        .filter(|live| installed(session, slot, live))
        .and_then(|live| live.track.as_ref())
        .map(|track| track.track_id.clone());
    let mappings = match slot {
        MediaSlot::RemoteAudio(_) => &mut snapshot.audio_mapping,
        MediaSlot::RemoteVideo(_) => &mut snapshot.video_mapping,
        _ => return false,
    };
    match source {
        Some(source) if mappings.get(&coordinate.media_index) != Some(&source) => {
            mappings.insert(coordinate.media_index, source);
            true
        }
        None => mappings.remove(&coordinate.media_index).is_some(),
        _ => false,
    }
}

pub(super) fn cancel_superseded(
    session: &mut Session,
    media: &BTreeMap<MediaSlot, MediaWant>,
    wanted: &mut BTreeMap<Resource, state::ResourceSpec>,
    effects: &mut VecDeque<Effect>,
) {
    let Some(pending) = session.media_allocation.as_mut() else {
        return;
    };
    if pending.receive.is_none() || matches!(pending.receive, Some(Stage::Cancelling)) {
        return;
    }
    let Some((slot, want)) = pending.wanted.first_key_value() else {
        return;
    };
    let failed = session
        .state
        .confirmed(&want.resource)
        .is_some_and(|confirmed| match confirmed.value.as_ref() {
            Some(state::confirmed_state::Value::Owned(live)) => {
                live.execution_failure.is_some()
                    || live.execution == state::ExecutionStatus::Failed as i32
            }
            None => confirmed.rejection.is_some(),
            Some(_) => true,
        });
    if media.get(slot) == Some(want)
        && !failed
        && !session
            .media_leases
            .get(slot)
            .is_some_and(|lease| lease.retired)
    {
        return;
    }
    pending.receive = Some(Stage::Cancelling);
    wanted.remove(&want.resource);
    if let Some(lease) = session.media_leases.get_mut(slot) {
        lease.retired = true;
    }
    effects.push_back(Effect::Rtc(RtcEffect::CancelMediaPreparation {
        generation: session.generation,
        operation: pending.operation,
    }));
}

pub(super) fn apply_plan(
    session: &mut Session,
    effects: &mut VecDeque<Effect>,
) -> Result<(), SignalingError> {
    let Some(pending) = session.media_allocation.as_mut() else {
        return Ok(());
    };
    if !matches!(pending.receive, Some(Stage::Offered)) {
        return Ok(());
    }
    let Some((slot, want)) = pending.wanted.first_key_value() else {
        return Ok(());
    };
    let Some(reference) = session.state.reference(&want.resource) else {
        return Ok(());
    };
    let Some(state::confirmed_state::Value::Owned(live)) = session
        .state
        .confirmed(&want.resource)
        .and_then(|confirmed| confirmed.value.as_ref())
    else {
        return Ok(());
    };
    let Some(binding) = live.binding.as_ref() else {
        return Ok(());
    };
    let Some(state::track_binding::Transport::Rtp(parameters)) = binding.transport.as_ref() else {
        return Err(SignalingError::Invalid("receive plan transport"));
    };
    let lease = session
        .media_leases
        .get(slot)
        .filter(|lease| !lease.retired)
        .ok_or(SignalingError::Invalid("receive plan lease"))?;
    let offer = lease
        .receive_offer
        .as_ref()
        .ok_or(SignalingError::Invalid("receive offer"))?;
    let kind = match slot.kind() {
        crate::MediaKind::Audio => state::TrackKind::Audio,
        crate::MediaKind::Video => state::TrackKind::Video,
    };
    state::validation::validate_receive_plan(offer, parameters, kind)
        .map_err(|_| SignalingError::Invalid("receive plan"))?;
    let slot = session
        .coordinates
        .get(slot)
        .and_then(Option::as_ref)
        .filter(|slot| slot.mid == offer.mid)
        .ok_or(SignalingError::Invalid("receive plan slot"))?;
    pending.receive = Some(Stage::Applying { reference });
    effects.push_back(Effect::Rtc(RtcEffect::ApplyReceivePlan {
        generation: session.generation,
        operation: pending.operation,
        slot: slot.clone(),
        parameters: parameters.clone(),
    }));
    Ok(())
}

pub(super) fn cleanup_complete(
    session: &mut Session,
    pending: &MediaAllocation,
    outcome: crate::client_sdp::ApplyOutcome,
    preparation: &Result<crate::client_sdp::Bindings, crate::client_sdp::Error>,
) -> bool {
    if pending.receive.is_none()
        || outcome != crate::client_sdp::ApplyOutcome::AppliedButCancelled
        || !preparation.as_ref().is_ok_and(|bindings| {
            !bindings.receptions.iter().any(|binding| {
                pending
                    .wanted
                    .keys()
                    .any(|slot| binding.key == crate::rtc_preparation::bootstrap_media_key(slot))
            })
        })
    {
        return false;
    }
    if let Some(Stage::Applying { reference }) = pending.receive {
        report(session, reference, None, false);
    }
    for (slot, want) in &pending.wanted {
        session.media_leases.remove(slot);
        session.coordinates.insert(slot.clone(), None);
        if let Some(Ok(cache)) = session.media_preparation.as_mut() {
            cache.remove(slot);
        }
        if session.media_wanted.get(slot) == Some(want) {
            session.media_blocked.insert(slot.clone(), want.clone());
        }
    }
    true
}

pub(super) fn report(
    session: &mut Session,
    reference: u64,
    observed: Option<&crate::rtc_preparation::PreparedRtp>,
    installed: bool,
) -> bool {
    let outcome = if installed {
        let Some(observed) = observed else {
            return false;
        };
        state::execution_report::Outcome::Installed(observed.binding.clone())
    } else {
        state::execution_report::Outcome::Failure(state::Failure {
            code: state::FailureCode::InternalFailure as i32,
            diagnostic: "receive preparation did not install the authoritative binding".into(),
        })
    };
    match session.state.report_execution(reference, outcome) {
        Ok(()) => installed,
        Err(error) => {
            log::warn!("Native receive execution report rejected: {error}");
            let _ = session.state.report_execution(
                reference,
                state::execution_report::Outcome::Failure(state::Failure {
                    code: state::FailureCode::InternalFailure as i32,
                    diagnostic: "receive installation evidence was rejected".into(),
                }),
            );
            false
        }
    }
}

impl Agent {
    pub(in crate::agent) fn media_offered(
        &mut self,
        generation: Generation,
        operation: OperationId,
        slot: SlotBinding,
        offer: state::RtpReceiveOffer,
    ) {
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation && !active.signaling_terminal)
        else {
            return;
        };
        let Some(pending) = active.media_allocation.as_mut().filter(|pending| {
            pending.operation == operation && matches!(pending.receive, Some(Stage::Offering))
        }) else {
            return;
        };
        let Some(want) = pending
            .wanted
            .get(&slot.slot)
            .filter(|want| active.media_wanted.get(&slot.slot) == Some(*want))
        else {
            return;
        };
        let kind = match slot.kind {
            crate::MediaKind::Audio => state::TrackKind::Audio,
            crate::MediaKind::Video => state::TrackKind::Video,
        };
        if slot.kind != slot.slot.kind()
            || slot.direction != slot.slot.direction()
            || slot.direction != crate::MediaDirection::ReceiveOnly
            || slot.mid != offer.mid
            || state::validation::validate_receive_offer(&offer, kind).is_err()
            || active.coordinates.iter().any(|(other, coordinate)| {
                coordinate.as_ref().is_some_and(|coordinate| {
                    if *other == slot.slot {
                        coordinate != &slot
                    } else {
                        coordinate.mid == slot.mid || coordinate.media_index == slot.media_index
                    }
                })
            })
        {
            return;
        }
        active
            .coordinates
            .insert(slot.slot.clone(), Some(slot.clone()));
        active.media_leases.insert(
            slot.slot.clone(),
            MediaLease {
                resource: want.resource.clone(),
                mid: slot.mid,
                source: want.source.clone(),
                receive_offer: Some(offer),
                identity_pending: false,
                retired: false,
                retirement_failed: false,
                wire_retired: false,
            },
        );
        if let Some(Ok(cache)) = active.media_preparation.as_mut() {
            cache.remove(&slot.slot);
        }
        pending.receive = Some(Stage::Offered);
        self.intent_dirty = true;
        self.send_intent_if_ready();
    }
}
