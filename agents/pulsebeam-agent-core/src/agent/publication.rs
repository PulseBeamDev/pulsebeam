use super::*;
use crate::ffi::{
    ErrorCode, ErrorInfo, PublicationAdmission, PublicationAdmissionStatus,
    PublicationConfigurationSnapshot, PublicationDemand, PublicationExecution, PublicationState,
};
use crate::native_state::Resource;
use pulsebeam_proto::state as wire;

impl Agent {
    /// Project the current input lifetime, independently of admission or receiver demand.
    pub fn pipe_state(&self, input: crate::PublicationInputId) -> Option<crate::ffi::PipeState> {
        self.resources.pipe_state(input)
    }

    pub fn publication_state(&self, owner: u64) -> Result<PublicationState, AgentError> {
        let owner = crate::TrackOwner(owner);
        let spec = self
            .resources
            .publication_spec(owner)
            .ok_or(crate::TrackResourceError::Released)?;
        let kind = spec
            .media_kind()
            .ok_or(crate::TrackResourceError::WrongKind)?;
        let configuration = self.resources.publication_configuration(owner)?;
        let input = self
            .resources
            .publication_inputs()
            .find(|control| control.operation.owner() == owner.id());
        let resource = Resource::Publication {
            kind: match kind {
                crate::MediaKind::Audio => wire::TrackKind::Audio,
                crate::MediaKind::Video => wire::TrackKind::Video,
            },
            label: spec.label().into(),
        };
        let mut admission = PublicationAdmission {
            status: PublicationAdmissionStatus::Pending,
            connection_generation: None,
            publication_id: None,
            evaluation_pending: true,
            error: None,
            execution: PublicationExecution::Unavailable,
            execution_error: None,
            demand: PublicationDemand::Unknown,
        };
        if let Some(active) = self.active.as_ref().filter(|active| {
            !active.signaling_terminal
                && !active.state.terminal()
                && active
                    .state
                    .session()
                    .is_some_and(|session| session.authorization.is_some())
        }) {
            admission.connection_generation = Some(active.generation.get());
            let confirmed = active.state.confirmed(&resource);
            let evaluation = active.state.evaluation(&resource);
            admission.evaluation_pending = evaluation.is_none();
            admission.error = evaluation
                .and_then(|state| state.rejection.as_ref())
                .map(publication_failure);
            if let Some(wire::confirmed_state::Value::Owned(live)) =
                confirmed.and_then(|state| state.value.as_ref())
            {
                admission.status = PublicationAdmissionStatus::Admitted;
                admission.publication_id = live.track.as_ref().map(|track| track.track_id.clone());
                admission.execution = match live.execution() {
                    wire::ExecutionStatus::Disabled => PublicationExecution::Disabled,
                    wire::ExecutionStatus::Installed => PublicationExecution::Installed,
                    wire::ExecutionStatus::Failed => PublicationExecution::Failed,
                    _ => PublicationExecution::Unavailable,
                };
                admission.execution_error =
                    live.execution_failure.as_ref().map(publication_failure);
                admission.demand = match live
                    .activity
                    .and_then(|value| wire::MediaActivity::try_from(value).ok())
                {
                    Some(wire::MediaActivity::Active) => PublicationDemand::Active,
                    Some(wire::MediaActivity::Idle) => PublicationDemand::Idle,
                    _ => PublicationDemand::Unknown,
                };
            } else if let Some(wire::confirmed_state::Value::Absent(absent)) =
                evaluation.and_then(|state| state.value.as_ref())
            {
                if absent.reason() == wire::AbsenceReason::PermissionRevoked {
                    admission.error = Some(ErrorInfo::new(
                        ErrorCode::PermissionDenied,
                        "Publication permission was revoked",
                    ));
                }
                if admission.error.is_some() {
                    admission.status = PublicationAdmissionStatus::Rejected;
                }
            }
        }
        Ok(PublicationState {
            observation_revision: 0,
            kind: kind.into(),
            label: spec.label().into(),
            closed: false,
            configuration: PublicationConfigurationSnapshot {
                desired_revision: configuration.desired_revision,
                evaluated_revision: configuration.evaluated_revision,
                accepted_revision: configuration.accepted_revision,
                enabled: configuration.desired_enabled,
                accepted_enabled: configuration.accepted_enabled,
                rejected_revision: configuration.rejected_revision,
                error: configuration.rejected_revision.map(|_| {
                    ErrorInfo::new(
                        ErrorCode::Internal,
                        "Publication configuration was not applied",
                    )
                }),
            },
            admission,
            input_attached: input
                .as_ref()
                .is_some_and(|control| control.operation.input().is_some()),
            forwarding_requested: input.is_some_and(|control| control.enabled),
        })
    }
}

pub(super) fn publication_failure(failure: &wire::Failure) -> ErrorInfo {
    let (code, message) = match failure.code() {
        wire::FailureCode::InvalidConfiguration => (
            ErrorCode::InvalidArgument,
            "Publication configuration was rejected",
        ),
        wire::FailureCode::PermissionDenied => (
            ErrorCode::PermissionDenied,
            "Publication permission was denied",
        ),
        wire::FailureCode::Incompatible => (
            ErrorCode::Unsupported,
            "Publication configuration is incompatible",
        ),
        wire::FailureCode::CapacityExceeded => (
            ErrorCode::CapacityExceeded,
            "Publication capacity was exceeded",
        ),
        wire::FailureCode::Unsupported => (
            ErrorCode::Unsupported,
            "Publication configuration is unsupported",
        ),
        wire::FailureCode::ResourceGone => (ErrorCode::Closed, "Publication wire lifetime ended"),
        _ => (ErrorCode::Internal, "Publication execution failed"),
    };
    ErrorInfo::new(code, message)
}
