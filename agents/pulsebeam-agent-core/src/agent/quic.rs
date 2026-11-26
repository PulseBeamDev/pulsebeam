use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use pulsebeam_proto::{
    prelude::Message,
    state::{self, validation},
};

use super::{
    Agent, AgentConfig, AgentError, AttemptMode, AttemptStage, Credential, Failure, FailureClass,
    NativeState, Session, SessionIdentity, native,
};
use crate::placement::{self, PendingPlacement, PlacementError};
use crate::{Effect, Generation, HttpResponse, OperationId, QuicAdmit, QuicEffect, QuicEvent};

#[derive(Clone, Copy)]
pub(super) enum PublicationProfile {
    LegacyH264Dd,
    Video { mime: &'static str, emits_dd: bool },
}

pub(super) enum Attempt {
    Placing {
        work: PendingPlacement,
        credential: Credential,
        successor: Option<Vec<u8>>,
    },
    Dialing {
        credential: Credential,
        owner: String,
    },
}

#[doc(hidden)]
pub struct QuicRtpPublication<'a> {
    pub publishes_h264_dd: bool,
    pub emits_video_dd: Option<bool>,
    pub mime_type: &'a str,
    pub generation: Generation,
    pub binding: core::num::NonZeroU16,
    pub ssrc: u32,
    pub payload_type: u8,
    pub ready: bool,
    pub publication_id: &'a str,
}

#[doc(hidden)]
pub struct QuicRtpReceiver<'a> {
    pub mime_type: &'a str,
    pub av1_whole_source: bool,
    pub video_dd: Option<i32>,
    pub generation: Generation,
    pub binding: core::num::NonZeroU16,
    pub incarnation: u64,
    pub ssrc: u32,
    pub payload_type: u8,
    pub ready: bool,
    pub publication_id: &'a str,
}

impl<'a> QuicRtpReceiver<'a> {
    fn new(
        session: &Session,
        binding: &'a state::TrackBinding,
        publication_id: &'a str,
        track: Option<&state::Track>,
        ready: bool,
    ) -> Option<Self> {
        let state::track_binding::Transport::QuicRtp(parameters) = binding.transport.as_ref()?
        else {
            return None;
        };
        let codec = parameters.codec.as_ref()?;
        Some(Self {
            av1_whole_source: track.is_some_and(|track| {
                matches!(track.kind.as_ref(), Some(state::track::Kind::Video(video)) if video.av1_whole_source)
            }),
            mime_type: &codec.format.as_ref()?.mime_type,
            video_dd: parameters.video_dd,
            ready,
            generation: session.generation,
            binding: core::num::NonZeroU16::new(u16::try_from(parameters.binding_id).ok()?)?,
            incarnation: parameters.incarnation,
            ssrc: parameters.primary_ssrc,
            payload_type: u8::try_from(codec.payload_type?).ok()?,
            publication_id,
        })
    }
}

fn logical_rtp_receiver<'a>(
    session: &'a Session,
    resource: &crate::native_state::Resource,
) -> Option<QuicRtpReceiver<'a>> {
    let (_, logical) = session.state.logical_offer(resource)?;
    if !validation::declarations_equivalent(
        session.state.target_spec(resource)?,
        logical.spec.as_ref()?,
    ) {
        return None;
    }
    let Some(state::logical_subscription_state::Execution::Installed(binding)) = &logical.execution
    else {
        return None;
    };
    let track = logical.source.as_ref()?;
    QuicRtpReceiver::new(session, binding, &track.publication_id, Some(track), true)
}

struct QuicFeatures {
    directional_video_dd: bool,
    data_record_dd: bool,
}

impl Agent {
    fn active_quic_media(&self) -> Option<&Session> {
        self.active.as_ref().filter(|session| {
            self.quic
                && self.desired.connected
                && !session.signaling_terminal
                && self.snapshot.terminal_failure.is_none()
        })
    }

    #[doc(hidden)]
    pub fn quic_rtp_receivers(
        &self,
        kind: crate::MediaKind,
    ) -> impl Iterator<Item = QuicRtpReceiver<'_>> {
        self.quic_rtp_receiver_bindings(kind)
            .filter(|binding| binding.ready)
    }

    #[doc(hidden)]
    pub fn quic_rtp_receiver_bindings(
        &self,
        kind: crate::MediaKind,
    ) -> impl Iterator<Item = QuicRtpReceiver<'_>> {
        use crate::native_state::Resource;
        use state::resource_spec::Declaration;
        self.active_quic_media()
            .into_iter()
            .flat_map(move |session| {
                let owned = session.state.owned().filter_map(move |(resource, live)| {
                    use crate::{MediaKind, MediaSlot};
                    if !matches!(
                        (kind, resource),
                        (
                            MediaKind::Audio,
                            Resource::Reception(MediaSlot::RemoteAudio(_))
                        ) | (
                            MediaKind::Video,
                            Resource::Reception(MediaSlot::RemoteVideo(_))
                        )
                    ) {
                        return None;
                    }
                    let spec = live.spec.as_ref()?;
                    let Some(Declaration::Subscription(subscription)) = spec.declaration.as_ref()
                    else {
                        return None;
                    };
                    let ready = live.execution == state::ExecutionStatus::Installed as i32
                        && session.state.target_spec(resource).is_some_and(|target| {
                            validation::declarations_equivalent(target, spec)
                                && target
                                    .settings
                                    .as_ref()
                                    .and_then(|settings| settings.enabled)
                                    == Some(true)
                        });
                    QuicRtpReceiver::new(
                        session,
                        subscription.binding.as_ref()?,
                        &subscription.publication_id,
                        live.track.as_ref(),
                        ready,
                    )
                });
                let logical = session.state.resources().filter_map(move |resource| {
                    use crate::{MediaKind, MediaSlot};
                    if !matches!(
                        (kind, resource),
                        (
                            MediaKind::Audio,
                            Resource::LogicalReception(MediaSlot::RemoteAudio(_))
                        ) | (
                            MediaKind::Video,
                            Resource::LogicalReception(MediaSlot::RemoteVideo(_))
                        )
                    ) {
                        return None;
                    }
                    logical_rtp_receiver(session, resource)
                });
                owned.chain(logical)
            })
    }

    #[doc(hidden)]
    pub fn quic_selector_receiver(
        &self,
        owner: crate::TrackOwner,
        source: &super::SelectorMatch,
    ) -> Result<Option<QuicRtpReceiver<'_>>, crate::ffi::ErrorInfo> {
        let Some(session) = self.active_quic_media() else {
            return Ok(None);
        };
        if !self.selectors.contains_match(owner, source, &session.state) {
            return Ok(None);
        }
        let kind = match source.kind {
            super::SelectorTrackKind::Audio => state::TrackKind::Audio,
            super::SelectorTrackKind::Video => state::TrackKind::Video,
            super::SelectorTrackKind::Data => return Ok(None),
        };
        let Some(resource) = native::logical_resource(session, &source.address(), kind) else {
            return Ok(None);
        };
        if let Some(confirmed) = session.state.confirmed(resource) {
            use state::logical_subscription_state::Execution;
            let failure =
                confirmed
                    .rejection
                    .as_ref()
                    .or_else(
                        || match &crate::native_state::logical_state(confirmed)?.execution {
                            Some(Execution::Failure(failure)) => Some(failure),
                            _ => None,
                        },
                    );
            if let Some(failure) = failure {
                use crate::ffi::{ErrorCode, ErrorInfo};
                use state::FailureCode;
                let code = match failure.code() {
                    FailureCode::CapacityExceeded => ErrorCode::CapacityExceeded,
                    FailureCode::Incompatible | FailureCode::Unsupported => ErrorCode::Unsupported,
                    FailureCode::InvalidConfiguration => ErrorCode::InvalidArgument,
                    FailureCode::PermissionDenied => ErrorCode::PermissionDenied,
                    FailureCode::ResourceGone => ErrorCode::Closed,
                    _ => ErrorCode::Internal,
                };
                return Err(ErrorInfo::new(code, "QUIC media reception failed"));
            }
        }
        Ok(logical_rtp_receiver(session, resource))
    }

    #[doc(hidden)]
    pub fn quic_rtp_publication(
        &self,
        kind: crate::MediaKind,
        track_id: &str,
    ) -> Option<QuicRtpPublication<'_>> {
        use state::{
            confirmed_state::Value, publication_spec::Kind, resource_spec::Declaration,
            track_binding::Transport,
        };
        let session = self.active_quic_media()?;
        let resource = crate::native_state::Resource::Publication {
            kind: match kind {
                crate::MediaKind::Audio => state::TrackKind::Audio,
                crate::MediaKind::Video => state::TrackKind::Video,
            },
            track_id: track_id.into(),
        };
        let target = session.state.target_spec(&resource)?;
        let Some(Value::Owned(live)) = &session.state.confirmed(&resource)?.value else {
            return None;
        };
        let live_spec = live.spec.as_ref()?;
        if !validation::declarations_equivalent(target, live_spec) {
            return None;
        }
        let Some(Declaration::Publication(publication)) = &live_spec.declaration else {
            return None;
        };
        let track_binding = match (kind, publication.kind.as_ref()?) {
            (crate::MediaKind::Audio, Kind::Audio(audio)) => audio.binding.as_ref()?,
            (crate::MediaKind::Video, Kind::Video(video)) => video.binding.as_ref()?,
            _ => return None,
        };
        let Some(Transport::QuicRtp(parameters)) = &track_binding.transport else {
            return None;
        };
        let binding = core::num::NonZeroU16::new(u16::try_from(parameters.binding_id).ok()?)?;
        let codec = parameters.codec.as_ref()?;
        let pt = u8::try_from(codec.payload_type?).ok()?;
        let emits_video_dd = match parameters.video_dd {
            Some(dd) => match state::QuicVideoDd::try_from(dd).ok()? {
                state::QuicVideoDd::Emit => Some(true),
                state::QuicVideoDd::Absent => Some(false),
                _ => return None,
            },
            None => None,
        };
        let ready = live.execution == state::ExecutionStatus::Installed as i32
            && target
                .settings
                .as_ref()
                .and_then(|settings| settings.enabled)
                == Some(true);
        Some(QuicRtpPublication {
            publishes_h264_dd: parameters.publishes_h264_dd,
            emits_video_dd,
            mime_type: &codec.format.as_ref()?.mime_type,
            generation: session.generation,
            binding,
            ssrc: parameters.primary_ssrc,
            payload_type: pt,
            ready,
            publication_id: &live.track.as_ref()?.publication_id,
        })
    }

    #[doc(hidden)]
    pub fn new_quic(config: AgentConfig) -> Result<Self, AgentError> {
        let mut agent = Self::new(config)?;
        agent.quic = true;
        Ok(agent)
    }

    #[doc(hidden)]
    pub fn enable_quic_h264_dd(&mut self, track_id: &str) -> Result<(), AgentError> {
        self.configure_quic_publication(track_id, PublicationProfile::LegacyH264Dd)
    }

    #[doc(hidden)]
    pub fn configure_quic_video(&mut self, track_id: &str, mime: &str) -> Result<(), AgentError> {
        let mime = match mime {
            "video/h264" => "video/h264",
            "video/vp8" => "video/vp8",
            "video/vp9" => "video/vp9",
            "video/av1" => "video/av1",
            _ => return Err(crate::ValidationError::QuicPublicationProfile.into()),
        };
        self.configure_quic_publication(
            track_id,
            PublicationProfile::Video {
                mime,
                emits_dd: false,
            },
        )
    }

    #[doc(hidden)]
    pub fn configure_quic_vp9_dd(&mut self, track_id: &str) -> Result<(), AgentError> {
        self.configure_quic_publication(
            track_id,
            PublicationProfile::Video {
                mime: "video/vp9",
                emits_dd: true,
            },
        )
    }

    fn configure_quic_publication(
        &mut self,
        track_id: &str,
        profile: PublicationProfile,
    ) -> Result<(), AgentError> {
        const MAX_TRACKS: usize = 64;
        if !self.quic || self.desired.connected || self.closing.is_some() {
            return Err(crate::ValidationError::QuicPublicationProfile.into());
        }
        crate::model::validate_track_id(track_id)?;
        if self.quic_publication_profiles.len() >= MAX_TRACKS
            && !self.quic_publication_profiles.contains_key(track_id)
        {
            return Err(crate::ValidationError::QuicPublicationProfileLimit.into());
        }
        self.quic_publication_profiles
            .insert(track_id.into(), profile);
        Ok(())
    }

    pub(super) fn start_quic_placement(&mut self, generation: Generation) {
        let Some(attempt) = self
            .attempt
            .as_mut()
            .filter(|attempt| attempt.generation == generation)
        else {
            return;
        };
        let credential = Credential {
            token: self.config.token.clone(),
            revision: self.config_credential_revision,
        };
        let continuation = if attempt.mode == AttemptMode::Replace && attempt.room.is_some() {
            let Some((owner, receipt)) = self
                .active
                .as_ref()
                .and_then(|active| active.quic_owner.as_ref())
                .zip(self.joined_successor.as_ref())
            else {
                self.fail_attempt(Failure {
                    class: FailureClass::ResourceExpired,
                    message: "QUIC continuation unavailable; rejoin explicitly".into(),
                });
                return;
            };
            Some((
                owner.clone(),
                receipt.proof.clone(),
                URL_SAFE_NO_PAD.encode(&receipt.proof),
            ))
        } else {
            None
        };
        let Ok(request) = placement::request(
            &self.config.endpoint,
            &credential.token,
            attempt.room.as_deref(),
            continuation
                .as_ref()
                .map(|(owner, _, encoded)| (owner.as_str(), encoded.as_str())),
        ) else {
            self.fail_attempt(Failure {
                class: FailureClass::InvalidConfiguration,
                message: "invalid QUIC placement request".into(),
            });
            return;
        };
        let (owner, successor) = continuation.map_or((None, None), |(owner, receipt, _)| {
            (Some(owner), Some(receipt))
        });
        let (work, effect) = PendingPlacement::issue(
            generation,
            self.ids.operation(),
            self.data_now,
            request,
            owner.as_deref(),
        );
        attempt.quic = Some(Attempt::Placing {
            work,
            credential,
            successor,
        });
        attempt.stage = AttemptStage::Acquiring;
        self.effects.push_back(Effect::Http(effect));
        if self.active.is_none() {
            self.snapshot.generation = Some(generation);
        }
        self.update_attempt_state();
    }

    fn take_placement(&mut self, operation: OperationId) -> Option<Attempt> {
        let attempt = self.attempt.as_mut()?;
        if matches!(&attempt.quic, Some(Attempt::Placing { work, .. }) if work.matches(operation)) {
            return attempt.quic.take();
        }
        None
    }

    pub(super) fn quic_placement_response(
        &mut self,
        operation: OperationId,
        response: &HttpResponse,
    ) -> bool {
        let Some(Attempt::Placing {
            work,
            credential,
            successor,
        }) = self.take_placement(operation)
        else {
            return false;
        };
        match work.complete(self.data_now, response) {
            Ok(placed) => {
                if let Some(attempt) = self.attempt.as_mut() {
                    attempt.quic = Some(Attempt::Dialing {
                        credential: credential.clone(),
                        owner: placed.placement.owner.clone(),
                    });
                    attempt.stage = AttemptStage::Requesting;
                    self.effects
                        .push_back(Effect::Quic(QuicEffect::Admit(QuicAdmit {
                            generation: placed.generation,
                            room: attempt.room.clone(),
                            placement: placed.placement,
                            deadline: placed.deadline,
                            token: credential.token,
                            successor,
                        })));
                }
                self.update_attempt_state();
            }
            Err(error) => self.fail_attempt(match error {
                PlacementError::Status(401 | 403) => Failure {
                    class: FailureClass::Authorization,
                    message: "QUIC placement denied".into(),
                },
                PlacementError::Status(429 | 503) | PlacementError::Expired => {
                    Failure::transient("QUIC placement unavailable")
                }
                _ => Failure::protocol("invalid QUIC placement response"),
            }),
        }
        true
    }

    pub(super) fn quic_placement_failed(&mut self, operation: OperationId) -> bool {
        if self.take_placement(operation).is_none() {
            return false;
        }
        self.fail_attempt(Failure::transient("QUIC placement request failed"));
        true
    }

    pub(super) fn retire_attempt_transport(&mut self, attempt: &mut super::Attempt) -> bool {
        let generation = attempt.generation;
        if !self.quic {
            self.effects
                .push_back(Effect::Rtc(crate::RtcEffect::Close { generation }));
            return true;
        }
        match attempt.quic.take() {
            Some(Attempt::Placing { work, .. }) => {
                self.effects.push_back(Effect::Http(work.cancel()));
            }
            Some(Attempt::Dialing { .. }) => {
                self.effects
                    .push_back(Effect::Quic(QuicEffect::Close { generation }));
                return true;
            }
            None => {}
        }
        false
    }

    pub(super) fn close_transport(&mut self, generation: Generation) {
        self.effects.push_back(if self.quic {
            Effect::Quic(QuicEffect::Close { generation })
        } else {
            Effect::Rtc(crate::RtcEffect::Close { generation })
        });
    }

    fn quic_accept(
        &mut self,
        generation: Generation,
        connection_id: String,
        participant_id: String,
        room: Option<String>,
        capabilities: &[u8],
        features: QuicFeatures,
    ) -> Result<(), AgentError> {
        let Some(attempt) = self
            .attempt
            .as_ref()
            .filter(|attempt| attempt.generation == generation)
        else {
            return Ok(());
        };
        let Some(Attempt::Dialing { credential, owner }) = attempt.quic.as_ref() else {
            return Ok(());
        };
        let Some(connection_id) = crate::ConnectionId::from_native_response(connection_id) else {
            self.fail_attempt(Failure::protocol("invalid QUIC connection ID"));
            return Ok(());
        };
        if attempt.room != room || !validation::is_application_name(&participant_id) {
            self.fail_attempt(Failure::protocol("QUIC admission identity mismatch"));
            return Ok(());
        }
        let Ok(rtp) = state::RtpCapabilities::decode(capabilities) else {
            self.fail_attempt(Failure::protocol("invalid QUIC RTP capabilities"));
            return Ok(());
        };
        if validation::validate_capabilities(&rtp).is_err() {
            self.fail_attempt(Failure::protocol("invalid QUIC RTP capabilities"));
            return Ok(());
        }
        let state = NativeState::with_profile(
            native::STATE_MEMORY_BYTES,
            room.as_ref().map(|room_id| state::Participant {
                participant_id: participant_id.clone(),
                room_id: room_id.clone(),
            }),
            validation::BindingProfile::Quic,
        )?;
        let join_revision = attempt.join_revision;
        let mut session = Session::new(
            generation,
            credential.clone(),
            SessionIdentity {
                connection_id,
                participant_id,
                room_id: room,
            },
            state,
            self.ids.channel(),
            crate::BootstrapCapabilities {
                rtp,
                data_delivery: alloc::vec::Vec::new(),
                data_v1: true,
            },
        );
        session.quic_owner = Some(owner.clone());
        session.quic_directional_video_dd = features.directional_video_dd;
        session.quic_data_record_dd = features.data_record_dd;
        session.reliable_data_enabled = true;
        self.attempt = None;
        self.activate_session(session, join_revision);
        self.refresh_resources()?;
        self.request_reconciliation();
        Ok(())
    }

    #[doc(hidden)]
    pub fn quic_data_receiver(
        &self,
        generation: Generation,
        binding: core::num::NonZeroU16,
    ) -> Option<(crate::ChannelId, bool, bool)> {
        self.active_quic_media()
            .filter(|active| active.generation == generation)?
            .payloads
            .iter()
            .find_map(|(&channel, physical)| {
                let state::DataV1Binding::Quic(parameters) = physical.data_v1 else {
                    return None;
                };
                (physical.offer.is_some() && parameters.binding_id == u32::from(binding.get()))
                    .then_some((
                        channel,
                        physical.state == super::native::PayloadState::Active
                            && physical.data_v1_receiver.is_some(),
                        parameters.record_dd,
                    ))
            })
    }

    pub(super) fn handle_quic(&mut self, event: QuicEvent) -> Result<(), AgentError> {
        match event {
            QuicEvent::Accepted {
                generation,
                connection_id,
                participant_id,
                room,
                capabilities,
                directional_video_dd,
                data_record_dd,
            } => {
                return self.quic_accept(
                    generation,
                    connection_id,
                    participant_id,
                    room,
                    &capabilities,
                    QuicFeatures {
                        directional_video_dd,
                        data_record_dd,
                    },
                );
            }
            QuicEvent::Control {
                generation,
                payload,
            } => {
                if self.active.as_ref().is_some_and(|active| {
                    active.generation == generation && !active.signaling_terminal
                }) && let Err(error) = self.apply_native(&payload)
                {
                    self.fail_terminal(Failure::protocol(error.to_string()));
                    return Err(error);
                }
            }
            QuicEvent::PictureLoss { .. } => {}
            QuicEvent::Data {
                generation,
                binding,
                payload,
            } => {
                if let Some((channel, true, _)) = self.quic_data_receiver(generation, binding) {
                    self.handle_data_channel(crate::DataChannelEvent::Message {
                        generation,
                        channel,
                        binary: true,
                        payload,
                    })?;
                }
            }
            QuicEvent::ControlSent {
                generation,
                operation,
            } => {
                if self.pending_signal.as_ref().is_some_and(|pending| {
                    pending.generation == generation && pending.operation == operation
                }) {
                    self.pending_signal = None;
                    self.request_reconciliation();
                }
            }
            QuicEvent::DataSent {
                generation,
                channel,
                operation,
                result,
            } => {
                if self.active.as_ref().is_some_and(|active| {
                    active.generation == generation
                        && active.payloads.get(&channel).is_some_and(|physical| {
                            matches!(
                                physical.data_v1,
                                pulsebeam_proto::state::DataV1Binding::Quic(_)
                            )
                        })
                }) {
                    let event = match result {
                        Ok(()) => crate::DataChannelEvent::Sent {
                            generation,
                            channel,
                            operation,
                        },
                        Err(message) => crate::DataChannelEvent::SendFailed {
                            generation,
                            channel,
                            operation,
                            message,
                        },
                    };
                    self.handle_data_channel(event)?;
                }
            }
            QuicEvent::Renewed {
                generation,
                operation,
                revision,
            } => {
                self.quic_renewal_result(generation, operation, revision);
            }
            QuicEvent::TransportLost {
                generation,
                failure,
            } => {
                if self.active.as_ref().is_some_and(|active| {
                    active.generation == generation && !active.signaling_terminal
                }) {
                    self.close_transport(generation);
                    if failure.class != FailureClass::Transient {
                        self.fail_terminal(failure);
                    } else {
                        self.fence_signaling(generation);
                        if self.desired.connected
                            && self.snapshot.terminal_failure.is_none()
                            && self.attempt.is_none()
                            && self.retry.is_none()
                        {
                            self.notify_failure(failure);
                            self.start_attempt(AttemptMode::Replace);
                        }
                    }
                }
            }
            QuicEvent::AdmissionFailed {
                generation,
                failure,
            } => {
                if self.attempt.as_ref().is_some_and(|attempt| {
                    attempt.generation == generation
                        && matches!(attempt.quic, Some(Attempt::Dialing { .. }))
                }) {
                    self.fail_attempt(failure);
                }
            }
            QuicEvent::Closed { generation } => {
                if let Some(closing) = self.closing.as_mut() {
                    closing.generations.remove(&generation);
                    self.finish_close_if_ready();
                }
            }
        }
        Ok(())
    }
}
