use super::*;
use crate::participant::policy::{ReceiverExecution, ResourceObservation};
use wire::logical_receiver_report::Outcome;
use wire::logical_subscription_state::Execution;

pub(super) struct Binding {
    pub(super) id: u64,
    pub(super) exact: wire::ResourceSpec,
    pub(super) data_v1_offer: Option<wire::DataV1ReceiverOffer>,
    pub(super) wanted_revision: u64,
    evaluated_revision: u64,
    pub(super) retiring: bool,
}

impl Binding {
    pub(super) fn needs_preparation(&self) -> bool {
        !self.retiring && self.evaluated_revision < self.wanted_revision
    }

    fn bytes() -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(std::mem::size_of::<(u64, u64)>())
            .saturating_add(std::mem::size_of::<u64>())
            .saturating_add(codec::MAX_FRAME_SIZE)
    }

    fn transport(&self) -> Option<&wire::TrackBinding> {
        match self.exact.declaration.as_ref()? {
            wire::resource_spec::Declaration::Subscription(exact) => exact.binding.as_ref(),
            _ => None,
        }
    }
}

impl NativeSession {
    pub(super) fn report_logical_receiver(
        &mut self,
        reference: u64,
        revision: u64,
        report: wire::LogicalReceiverReport,
        unix_now: u64,
    ) -> Result<(), OutputError> {
        let key = Key::Owned(reference);
        let Some(row) = self.rows.get(&key) else {
            return Ok(());
        };
        let (Some(owned), Some(wire::confirmed_state::Value::LogicalSubscription(state))) =
            (&row.owned, &row.value.value)
        else {
            return Ok(());
        };
        let (Some(spec), Some(source)) = (&state.spec, &state.source) else {
            return Ok(());
        };
        if owned.retired
            || state.source_revision != report.source_revision
            || matches!(state.execution, Some(Execution::Failure(_)))
            || !self.authorization_current(unix_now)
            || !self.permissions.permits_intent(spec)
        {
            return Ok(());
        }
        match report.outcome {
            Some(Outcome::Failure(error)) => self.fail_logical_binding(reference, error),
            Some(Outcome::Prepared(transport)) if owned.logical_binding.is_none() => {
                let address = observed_address(spec).ok_or(OutputError::InvalidObservation)?;
                let is_data = address.kind == wire::TrackKind::Data as i32;
                if matches!(
                    transport.transport,
                    Some(
                        wire::track_binding::Transport::SctpDataV1(_)
                            | wire::track_binding::Transport::QuicDataV1(_)
                    )
                ) != is_data
                    || is_data != state.data_v1.is_some()
                {
                    return self.fail_logical_binding(
                        reference,
                        failure(wire::FailureCode::InvalidConfiguration),
                    );
                }
                let data_v1_offer = state.data_v1.clone();
                let exact = wire::ResourceSpec {
                    settings: spec.settings.clone(),
                    declaration: Some(wire::resource_spec::Declaration::Subscription(
                        wire::SubscriptionSpec {
                            publication_id: source.publication_id.clone(),
                            source_room: source.room_id.clone(),
                            kind: address.kind,
                            binding: Some(transport),
                        },
                    )),
                };
                if validation::validate_spec_for(&exact, self.binding_profile).is_err() {
                    return self.fail_logical_binding(
                        reference,
                        failure(wire::FailureCode::InvalidConfiguration),
                    );
                }
                if !live_fits(&exact) || self.charge(0, Binding::bytes()).is_err() {
                    return self.fail_logical_binding(
                        reference,
                        failure(wire::FailureCode::CapacityExceeded),
                    );
                }
                // A report revision cannot also be an owned reference: the client
                // consumes both from the same monotonic input clock. Forwarding uses
                // this nonreusable physical key, not the public subscription ref.
                self.logical_bindings.insert(revision, reference);
                let owned = self
                    .rows
                    .get_mut(&key)
                    .and_then(|row| row.owned.as_mut())
                    .ok_or(OutputError::InvalidObservation)?;
                owned.logical_binding = Some(Box::new(Binding {
                    id: revision,
                    exact,
                    data_v1_offer,
                    wanted_revision: revision,
                    evaluated_revision: 0,
                    retiring: false,
                }));
                self.queue_preparation(reference);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(super) fn request_logical_retirement(&mut self, reference: u64) -> bool {
        let Some(binding) = self
            .rows
            .get_mut(&Key::Owned(reference))
            .and_then(|row| row.owned.as_mut())
            .and_then(|owned| owned.logical_binding.as_mut())
        else {
            return false;
        };
        if !binding.retiring {
            binding.retiring = true;
            self.logical_retirements.push_back(binding.id);
        }
        true
    }

    pub(super) fn reconcile_logical_source(
        &mut self,
        reference: u64,
        state: &mut wire::LogicalSubscriptionState,
        resolved: Option<(wire::Track, Option<std::num::NonZeroU64>)>,
        branches: &DataBranchIds,
    ) -> Result<(), OutputError> {
        let capture_clock = resolved.as_ref().and_then(|(track, source)| {
            source.and_then(|_| match &track.kind {
                Some(wire::track::Kind::Data(data)) => data.capture_clock.as_ref(),
                _ => None,
            })
        });
        let changed = state.source.as_ref().map(|track| &track.publication_id)
            != resolved.as_ref().map(|(track, _)| &track.publication_id)
            || state.data_v1.as_ref().map(|offer| offer.source)
                != resolved
                    .as_ref()
                    .and_then(|(_, source)| source.map(std::num::NonZeroU64::get))
            || state
                .data_v1
                .as_ref()
                .and_then(|offer| offer.capture_clock.as_ref())
                != capture_clock;
        let capture_clock = capture_clock.cloned();
        if changed && self.request_logical_retirement(reference) {
            return set_logical_source(state, None, None);
        }
        let Some((track, fabric_source)) = resolved else {
            return set_logical_source(state, None, None);
        };
        let offer = if changed {
            if let Some(source) = fabric_source {
                Some(wire::DataV1ReceiverOffer {
                    source: source.get(),
                    branch: branches.allocate()?,
                    capture_clock,
                })
            } else {
                None
            }
        } else {
            state.data_v1.clone()
        };
        set_logical_source(state, Some(track), offer)
    }

    pub(crate) fn take_logical_retirement(
        &mut self,
    ) -> Result<Option<ExecutionCommand>, OutputError> {
        self.logical_retirements
            .pop_front()
            .map(|binding| self.retirement_command(binding))
            .transpose()
    }

    pub(crate) fn logical_retirement_pending(&self) -> bool {
        !self.logical_retirements.is_empty()
    }

    fn clear_logical_offer(&mut self, reference: u64) -> Result<(), OutputError> {
        let key = Key::Owned(reference);
        let Some(row) = self.rows.get(&key) else {
            return Ok(());
        };
        let mut value = row.value.clone();
        let Some(wire::confirmed_state::Value::LogicalSubscription(state)) = &mut value.value
        else {
            return Err(OutputError::InvalidObservation);
        };
        set_logical_source(state, None, None)?;
        self.put(key, value)
    }

    fn project_logical_execution(
        &mut self,
        reference: u64,
        execution: Option<Execution>,
    ) -> Result<(), OutputError> {
        let key = Key::Owned(reference);
        let Some(row) = self.rows.get(&key) else {
            return Ok(());
        };
        if row.owned.as_ref().is_none_or(|owned| owned.retired) {
            return Ok(());
        }
        let mut value = row.value.clone();
        let Some(wire::confirmed_state::Value::LogicalSubscription(state)) = &mut value.value
        else {
            return Err(OutputError::InvalidObservation);
        };
        if state.source.is_some() {
            state.execution = execution;
            self.put(key, value)?;
        }
        Ok(())
    }

    fn fail_logical_binding(
        &mut self,
        reference: u64,
        error: wire::Failure,
    ) -> Result<(), OutputError> {
        self.request_logical_retirement(reference);
        self.project_logical_execution(reference, Some(Execution::Failure(error)))
    }

    pub(super) fn complete_logical(
        &mut self,
        work: Preparation,
        result: Result<ReceiverExecution, wire::Failure>,
        unix_now: u64,
    ) -> Result<(), OutputError> {
        let reference = work.owned();
        let key = Key::Owned(reference);
        let current = self.preparation_spec(&work).is_some_and(|spec| {
            result.as_ref().map_or(true, |execution| {
                crate::control::native::receiver_target(spec)
                    .is_ok_and(|target| target.source == execution.source)
            })
        });
        let authorized = self.authorization_current(unix_now);
        let Some(owned) = self.rows.get_mut(&key).and_then(|row| row.owned.as_mut()) else {
            return Ok(());
        };
        if owned.active != Some(work.stamp()) {
            return Ok(());
        }
        owned.active = None;
        let binding = owned
            .logical_binding
            .as_mut()
            .ok_or(OutputError::InvalidObservation)?;
        if current && authorized {
            binding.evaluated_revision = work.revision;
        }
        let retired = owned.retired;
        let transport = binding.transport().cloned();
        let policy_update = work.revision != binding.id;
        let settings = policy_update.then(|| binding.exact.settings.clone());
        let receiver_available = owned
            .receiver_binding
            .is_some_and(|binding| binding.available);
        self.charge(Preparation::bytes(), 0)?;
        if !current || !authorized {
            if result.is_ok() && !retired {
                self.request_logical_retirement(reference);
                self.clear_logical_offer(reference)?;
            } else {
                self.queue_preparation(reference);
            }
            return Ok(());
        }
        if let Some(settings) = settings {
            let mut value = self
                .rows
                .get(&key)
                .ok_or(OutputError::InvalidObservation)?
                .value
                .clone();
            let Some(wire::confirmed_state::Value::LogicalSubscription(state)) = &mut value.value
            else {
                return Err(OutputError::InvalidObservation);
            };
            let spec = state.spec.as_mut().ok_or(OutputError::InvalidObservation)?;
            value.evaluated_revision = work.revision;
            value.rejection = result.as_ref().err().cloned();
            if result.is_ok() {
                spec.settings = settings;
            }
            let keep_installed = receiver_available
                && matches!(state.execution, Some(Execution::Installed(_)))
                && result.as_ref().is_err_and(|error| {
                    matches!(
                        error.code(),
                        wire::FailureCode::Unsupported | wire::FailureCode::InvalidConfiguration
                    )
                });
            self.put(key, value)?;
            if keep_installed {
                self.queue_preparation(reference);
                return Ok(());
            }
        }
        match result {
            Ok(execution) if execution.failed => {
                self.fail_logical_binding(reference, failure(wire::FailureCode::InternalFailure))?;
            }
            Ok(execution) => {
                let installed = if execution.enabled && execution.ready {
                    Some(Execution::Installed(
                        transport.ok_or(OutputError::InvalidObservation)?,
                    ))
                } else {
                    None
                };
                self.project_logical_execution(reference, installed)?;
                self.queue_preparation(reference);
            }
            Err(error) => self.fail_logical_binding(reference, error)?,
        }
        Ok(())
    }

    pub(super) fn observe_logical_resource(
        &mut self,
        reference: u64,
        observation: ResourceObservation,
    ) -> Result<(), OutputError> {
        let key = Key::Owned(reference);
        let row = self
            .rows
            .get_mut(&key)
            .ok_or(OutputError::InvalidObservation)?;
        let owned = row.owned.as_mut().ok_or(OutputError::InvalidObservation)?;
        let binding = owned
            .logical_binding
            .as_ref()
            .ok_or(OutputError::InvalidObservation)?;
        match observation {
            ResourceObservation::Released { binding: id } => {
                if !binding.retiring {
                    return Err(OutputError::InvalidObservation);
                }
                let active = owned.active.take().is_some();
                let terminal = owned.retired;
                let needs_evaluation = row.value.evaluated_revision < owned.target.revision;
                let revision = owned.target.revision;
                owned.logical_binding = None;
                owned.receiver_binding = None;
                self.logical_bindings.remove(&id);
                self.charge(
                    Binding::bytes().saturating_add(if active { Preparation::bytes() } else { 0 }),
                    0,
                )?;
                self.reserve_execution(reference, 0)?;
                if terminal {
                    self.cutoff_complete(reference)?;
                } else {
                    if needs_evaluation {
                        self.project_observed_intent(reference, revision)
                            .map_err(|_| OutputError::Capacity)?;
                    }
                    self.source_lookup_changed();
                }
            }
            ResourceObservation::StorageReleased { sequence, .. } => {
                if sequence != 0 && owned.storage_sequence == sequence {
                    self.reserve_execution(reference, 0)?;
                }
            }
            _ if binding.retiring || owned.retired => {}
            ResourceObservation::ReceiverBinding { state, .. } => owned.receiver_binding = state,
            ResourceObservation::Ended { .. } => {
                self.fail_logical_binding(reference, failure(wire::FailureCode::ResourceGone))?;
                self.source_lookup_changed();
            }
            ResourceObservation::Receiver { execution, .. } if binding.evaluated_revision != 0 => {
                if execution.failed {
                    self.fail_logical_binding(
                        reference,
                        failure(wire::FailureCode::InternalFailure),
                    )?;
                } else {
                    let installed = if execution.enabled && execution.ready {
                        Some(Execution::Installed(
                            binding
                                .transport()
                                .cloned()
                                .ok_or(OutputError::InvalidObservation)?,
                        ))
                    } else {
                        None
                    };
                    self.project_logical_execution(reference, installed)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
