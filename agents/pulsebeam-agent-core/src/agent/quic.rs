use alloc::string::{String, ToString};
use pulsebeam_proto::state::{self, validation};

use super::{
    Agent, AgentConfig, AgentError, AttemptStage, Credential, Failure, FailureClass, NativeState,
    Session, SessionIdentity, native,
};
use crate::placement::{self, PendingPlacement, PlacementError};
use crate::{Effect, Generation, HttpResponse, OperationId, QuicAdmit, QuicEffect, QuicEvent};

pub(super) enum Attempt {
    Placing {
        work: PendingPlacement,
        credential: Credential,
    },
    Dialing(Credential),
}

impl Agent {
    #[doc(hidden)]
    pub fn new_quic(config: AgentConfig) -> Result<Self, AgentError> {
        let mut agent = Self::new(config)?;
        agent.quic = true;
        Ok(agent)
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
        let Ok(request) = placement::request(
            &self.config.endpoint,
            &credential.token,
            attempt.room.as_deref(),
            None,
        ) else {
            self.fail_attempt(Failure {
                class: FailureClass::InvalidConfiguration,
                message: "invalid QUIC placement request".into(),
            });
            return;
        };
        let (work, effect) = PendingPlacement::issue(
            generation,
            self.ids.operation(),
            self.data_now,
            request,
            None,
        );
        attempt.quic = Some(Attempt::Placing { work, credential });
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
        let Some(Attempt::Placing { work, credential }) = self.take_placement(operation) else {
            return false;
        };
        match work.complete(self.data_now, response) {
            Ok(placed) => {
                if let Some(attempt) = self.attempt.as_mut() {
                    attempt.quic = Some(Attempt::Dialing(credential.clone()));
                    attempt.stage = AttemptStage::Requesting;
                    self.effects
                        .push_back(Effect::Quic(QuicEffect::Admit(QuicAdmit {
                            generation: placed.generation,
                            room: attempt.room.clone(),
                            placement: placed.placement,
                            deadline: placed.deadline,
                            token: credential.token,
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
            Some(Attempt::Dialing(_)) => {
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
    ) -> Result<(), AgentError> {
        let Some(attempt) = self
            .attempt
            .as_ref()
            .filter(|attempt| attempt.generation == generation)
        else {
            return Ok(());
        };
        let Some(Attempt::Dialing(credential)) = attempt.quic.as_ref() else {
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
        let state = NativeState::with_profile(
            native::STATE_MEMORY_BYTES,
            room.as_ref().map(|room_id| state::Participant {
                participant_id: participant_id.clone(),
                room_id: room_id.clone(),
            }),
            validation::BindingProfile::Quic,
        )?;
        let join_revision = attempt.join_revision;
        let session = Session::new(
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
                rtp: state::RtpCapabilities::default(),
                data_delivery: alloc::vec::Vec::new(),
                data_v1: false,
            },
        );
        self.attempt = None;
        self.activate_session(session, join_revision);
        self.refresh_resources()?;
        self.request_reconciliation();
        Ok(())
    }

    pub(super) fn handle_quic(&mut self, event: QuicEvent) -> Result<(), AgentError> {
        match event {
            QuicEvent::Accepted {
                generation,
                connection_id,
                participant_id,
                room,
            } => {
                return self.quic_accept(generation, connection_id, participant_id, room);
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
                    self.fail_terminal(failure);
                }
            }
            QuicEvent::AdmissionFailed {
                generation,
                failure,
            } => {
                if self.attempt.as_ref().is_some_and(|attempt| {
                    attempt.generation == generation
                        && matches!(attempt.quic, Some(Attempt::Dialing(_)))
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
