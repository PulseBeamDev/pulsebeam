use super::*;

impl Target {
    pub(in crate::tests) fn receive_plan(&self) -> Option<state::TrackBinding> {
        let state::resource_spec::Declaration::Subscription(subscription) =
            self.spec().declaration.as_ref()?
        else {
            return None;
        };
        let offer = subscription.rtp_offer.as_ref()?;
        let codec = offer.codecs.first().unwrap().clone();
        let primary = 10_000 + u32::try_from(self.reference).unwrap() * 2;
        Some(state::TrackBinding {
            transport: Some(state::track_binding::Transport::Rtp(state::RtpParameters {
                mid: offer.mid.clone(),
                encodings: vec![state::RtpEncodingParameters {
                    ssrc: Some(primary),
                    rtx_ssrc: codec.rtx_payload_type.map(|_| primary + 1),
                    ..Default::default()
                }],
                codec: Some(codec),
                header_extensions: offer.header_extensions.clone(),
                rtcp: Some(state::RtcpParameters {
                    cname: alloc::format!("receiver-{}", self.reference),
                    reduced_size: false,
                }),
            })),
        })
    }
}

pub(in crate::tests) fn prepare_receivers(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
    participant: &state::Participant,
    tracks: &[state::Track],
) -> Vec<Target> {
    let slots = resources(channel)
        .slots
        .into_iter()
        .filter(|slot| slot.direction == MediaDirection::ReceiveOnly)
        .collect::<Vec<_>>();
    prepare(agent, generation, channel, participant, tracks, &slots)
}

pub(in crate::tests) fn prepare_reception(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
    participant: &state::Participant,
    tracks: &[state::Track],
    slot: SlotBinding,
) -> Target {
    prepare(agent, generation, channel, participant, tracks, &[slot]).remove(0)
}

fn prepare(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
    participant: &state::Participant,
    tracks: &[state::Track],
    slots: &[SlotBinding],
) -> Vec<Target> {
    let mut offered = None;
    let mut targets = BTreeMap::new();
    let mut completed = Vec::new();
    let mut applied = alloc::collections::BTreeSet::new();
    for _ in 0..slots.len() * 5 {
        if completed.len() == slots.len() {
            return completed;
        }
        match next_effect(agent) {
            Effect::Rtc(RtcEffect::OfferReceiveMedia {
                generation: actual,
                operation,
                target,
            }) => {
                assert_eq!(actual, generation);
                assert!(offered.is_none());
                let slot = match target {
                    crate::ReceiveTarget::Existing(slot) => {
                        assert!(slots.contains(&slot));
                        slot
                    }
                    crate::ReceiveTarget::Allocate(allocation) => {
                        let slot = slots
                            .iter()
                            .find(|slot| slot.slot == allocation.slot)
                            .unwrap();
                        assert_eq!(allocation.format, Some(format(slot.kind)));
                        slot.clone()
                    }
                };
                let physical = OfferResources {
                    slots: vec![slot.clone()],
                    ..resources(channel)
                };
                let (bindings, capabilities) = prepared_media(&physical);
                let prepared = rtc_preparation::prepare_bootstrap_bindings(
                    bindings,
                    &physical.slots,
                    &capabilities,
                )
                .unwrap();
                let Some(state::track_binding::Transport::Rtp(parameters)) =
                    &prepared[&slot.slot].as_ref().unwrap().binding.transport
                else {
                    panic!("receiver parameters")
                };
                let offer = state::RtpReceiveOffer {
                    mid: slot.mid.clone(),
                    codecs: vec![parameters.codec.clone().unwrap()],
                    header_extensions: parameters.header_extensions.clone(),
                    rtcp_modes: vec![state::RtcpMode::Compound.as_str().into()],
                };
                agent
                    .handle(HostEvent::Rtc(RtcEvent::MediaOffered {
                        generation,
                        operation,
                        slot: slot.clone(),
                        offer,
                    }))
                    .unwrap();
                offered = Some((operation, slot));
            }
            Effect::DataChannel(DataChannelEffect::Send {
                operation,
                generation: actual,
                channel: actual_channel,
                payload,
                ..
            }) => {
                assert_eq!((actual, actual_channel), (generation, channel));
                let batch = codec::decode_client(&payload).unwrap();
                acknowledge_send(agent, generation, channel, operation);
                for record in batch.records {
                    if let Some(desired) = record.desired {
                        let target = Target {
                            reference: record.owned,
                            desired,
                        };
                        let Some(state::resource_spec::Declaration::Subscription(subscription)) =
                            &target.spec().declaration
                        else {
                            panic!("receive desire")
                        };
                        assert!(subscription.binding.is_none() && subscription.rtp_offer.is_some());
                        let preparing = target.receiver_preparing(participant, tracks);
                        native_connection::observe(agent, generation, channel, vec![preparing]);
                        assert!(targets.insert(target.reference, target).is_none());
                    }
                    if let Some(report) = record.execution {
                        assert!(
                            applied.remove(&record.owned),
                            "report must follow platform completion"
                        );
                        let target = targets.remove(&record.owned).unwrap();
                        assert!(report.revision > target.desired.revision);
                        assert_eq!(
                            report.outcome,
                            Some(state::execution_report::Outcome::Installed(
                                target.receive_plan().unwrap()
                            ))
                        );
                        completed.push(target);
                    }
                }
            }
            Effect::Rtc(RtcEffect::ApplyReceivePlan {
                generation: actual,
                operation,
                slot,
                parameters,
            }) => {
                assert_eq!(actual, generation);
                assert_eq!(offered.take(), Some((operation, slot.clone())));
                let target = targets
                    .values()
                    .find(|target| {
                        matches!(&target.receive_plan().unwrap().transport,
                    Some(state::track_binding::Transport::Rtp(plan)) if plan == &parameters)
                    })
                    .unwrap();
                assert!(applied.insert(target.reference));
                let Some(state::track_binding::Transport::Rtp(plan)) =
                    target.receive_plan().unwrap().transport
                else {
                    panic!("receive plan")
                };
                let (mut binding, _) = rtc_preparation::binding_fixture(format(slot.kind));
                binding.key = rtc_preparation::bootstrap_media_key(&slot.slot);
                binding.mid = slot.mid.clone();
                binding.codec.payload_type = plan
                    .codec
                    .as_ref()
                    .unwrap()
                    .payload_type
                    .unwrap()
                    .try_into()
                    .unwrap();
                binding.encodings[0].primary = plan.encodings[0].ssrc;
                binding.encodings[0].repair = plan.encodings[0].rtx_ssrc;
                binding.rtcp.cname = Some(plan.rtcp.as_ref().unwrap().cname.clone());
                agent
                    .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
                        generation,
                        operation,
                        slots: vec![slot],
                        outcome: client_sdp::ApplyOutcome::Applied,
                        preparation: Ok(client_sdp::Bindings {
                            publications: Vec::new(),
                            receptions: vec![binding],
                        }),
                    }))
                    .unwrap();
            }
            effect => panic!("receive lifecycle effect: {effect:?}"),
        }
    }
    panic!("receive lifecycle did not complete");
}
