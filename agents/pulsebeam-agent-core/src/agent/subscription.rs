use super::*;
use crate::ffi::{
    AudioSubscriptionState, DataPublicationState, DataSubscriptionState, EffectiveMediaFormat,
    ErrorInfo, PublicationAdmissionStatus, PublicationExecution, SubscriptionSourceStatus,
    VideoSubscriptionState,
};
use crate::native_state::Resource;
use pulsebeam_proto::state as wire;

#[cfg(test)]
mod tests;

impl Agent {
    pub fn video_subscription_state(
        &self,
        owner: u64,
    ) -> Result<VideoSubscriptionState, AgentError> {
        let owner = crate::TrackOwner(owner);
        let (selector, configuration) = self
            .resources
            .video_view(owner)
            .ok_or(crate::TrackResourceError::Released)?;
        let intent_revision = self.resources.intent_revision(owner).unwrap_or(0);
        let (source_status, source_track_id, mut error) =
            source_state(self, selector, wire::TrackKind::Video);
        let (evaluated_revision, accepted_revision, rejected_revision) = receiver_revisions(
            self,
            selector,
            wire::TrackKind::Video,
            source_track_id.as_deref(),
        );
        let (admission, admission_pending, execution, execution_error) = receiver_outcome(
            self,
            selector,
            wire::TrackKind::Video,
            source_track_id.as_deref(),
        );
        if error.is_none() {
            error = admission_error(
                self,
                selector,
                wire::TrackKind::Video,
                source_track_id.as_deref(),
            );
        }
        Ok(VideoSubscriptionState {
            observation_revision: 0,
            intent_revision,
            evaluated_revision,
            accepted_revision,
            rejected_revision,
            selector: selector.into(),
            configuration: configuration.clone().into(),
            local_enabled: configuration.visible,
            closed: false,
            source_status,
            source_format: source_format(self, source_track_id.as_deref()),
            source_track_id,
            admission,
            admission_pending,
            error,
            execution,
            execution_error,
        })
    }

    pub fn audio_subscription_state(
        &self,
        owner: u64,
    ) -> Result<AudioSubscriptionState, AgentError> {
        let owner = crate::TrackOwner(owner);
        let selector = self
            .resources
            .audio_view(owner)
            .ok_or(crate::TrackResourceError::Released)?;
        let intent_revision = self.resources.intent_revision(owner).unwrap_or(0);
        let (source_status, source_track_id, mut error) =
            source_state(self, selector, wire::TrackKind::Audio);
        let (evaluated_revision, accepted_revision, rejected_revision) = receiver_revisions(
            self,
            selector,
            wire::TrackKind::Audio,
            source_track_id.as_deref(),
        );
        let (admission, admission_pending, execution, execution_error) = receiver_outcome(
            self,
            selector,
            wire::TrackKind::Audio,
            source_track_id.as_deref(),
        );
        if error.is_none() {
            error = admission_error(
                self,
                selector,
                wire::TrackKind::Audio,
                source_track_id.as_deref(),
            );
        }
        Ok(AudioSubscriptionState {
            observation_revision: 0,
            intent_revision,
            evaluated_revision,
            accepted_revision,
            rejected_revision,
            selector: selector.into(),
            local_enabled: true,
            closed: false,
            source_status,
            source_format: source_format(self, source_track_id.as_deref()),
            source_track_id,
            admission,
            admission_pending,
            error,
            execution,
            execution_error,
        })
    }

    pub fn data_subscription_state(&self, owner: u64) -> Result<DataSubscriptionState, AgentError> {
        let owner = crate::TrackOwner(owner);
        let (subscriber, failure) = self
            .resources
            .data_receiver(owner)
            .ok_or(crate::TrackResourceError::Released)?;
        let selector = crate::TrackSelector {
            room: subscriber.room.clone(),
            participant_id: subscriber.participant_id.clone(),
            track_id: subscriber.topic.clone(),
        };
        let intent_revision = self.resources.intent_revision(owner).unwrap_or(0);
        let (mut source_status, source_track_id, mut error) =
            source_state(self, &selector, wire::TrackKind::Data);
        let terminal_error = failure.and_then(receive_failure);
        if source_status == SubscriptionSourceStatus::Pending
            && let Some(cause) = terminal_error.as_ref()
        {
            source_status = SubscriptionSourceStatus::Rejected;
            error = Some(cause.clone());
        }
        let (evaluated_revision, accepted_revision, rejected_revision) = receiver_revisions(
            self,
            &selector,
            wire::TrackKind::Data,
            source_track_id.as_deref(),
        );
        let (admission, admission_pending, execution, execution_error) = receiver_outcome(
            self,
            &selector,
            wire::TrackKind::Data,
            source_track_id.as_deref(),
        );
        if error.is_none() {
            error = admission_error(
                self,
                &selector,
                wire::TrackKind::Data,
                source_track_id.as_deref(),
            );
        }
        Ok(DataSubscriptionState {
            observation_revision: 0,
            intent_revision,
            evaluated_revision,
            accepted_revision,
            rejected_revision,
            selector: selector.into(),
            delivery: subscriber.mode.into(),
            local_enabled: true,
            closed: false,
            source_status,
            source_track_id,
            admission,
            admission_pending,
            error,
            execution,
            execution_error,
            terminal_error,
        })
    }

    pub fn data_publication_state(&self, owner: u64) -> Result<DataPublicationState, AgentError> {
        let owner = crate::TrackOwner(owner);
        let (track_id, delivery) = self
            .resources
            .data_publication(owner)
            .ok_or(crate::TrackResourceError::Released)?;
        let intent_revision = self.resources.intent_revision(owner).unwrap_or(0);
        let (evaluated_revision, accepted_revision, rejected_revision) =
            publication_revisions(self, track_id);
        let (admission, admission_pending, error, execution, execution_error) =
            publication_outcome(self, track_id);
        Ok(DataPublicationState {
            observation_revision: 0,
            intent_revision,
            evaluated_revision,
            accepted_revision,
            rejected_revision,
            track_id: track_id.into(),
            delivery: delivery.into(),
            local_enabled: true,
            closed: false,
            admission,
            admission_pending,
            error,
            execution,
            execution_error,
        })
    }
}

fn active_agent(agent: &Agent) -> Option<&super::Session> {
    agent.active.as_ref().filter(|active| {
        !active.signaling_terminal
            && !active.state.terminal()
            && active
                .state
                .session()
                .is_some_and(|session| session.authorization.is_some())
    })
}

fn permission_revoked(state: &wire::ConfirmedState) -> bool {
    matches!(state.value.as_ref(), Some(wire::confirmed_state::Value::Absent(absent))
        if absent.reason() == wire::AbsenceReason::PermissionRevoked)
}

fn is_receiver_resource(
    resource: &Resource,
    kind: wire::TrackKind,
    publication_id: Option<&str>,
) -> bool {
    match (resource, kind) {
        (Resource::Reception(_), wire::TrackKind::Audio | wire::TrackKind::Video) => {
            publication_id.is_some()
        }
        (Resource::DataSubscription(id), wire::TrackKind::Data) => {
            publication_id.is_some_and(|track| track == id)
        }
        _ => false,
    }
}

fn receiver_revisions(
    agent: &Agent,
    selector: &crate::TrackSelector,
    kind: wire::TrackKind,
    publication_id: Option<&str>,
) -> (u64, Option<u64>, Option<u64>) {
    let Some(active) = active_agent(agent) else {
        return (0, None, None);
    };
    if let Some(resource) = super::native::logical_resource(active, selector, kind) {
        let evaluation = active.state.evaluation(resource);
        let confirmed = active.state.confirmed(resource);
        return (
            evaluation
                .or(confirmed)
                .map_or(0, |state| state.evaluated_revision),
            confirmed
                .filter(|state| crate::native_state::logical_state(state).is_some())
                .map(|state| state.evaluated_revision),
            evaluation
                .filter(|state| state.rejection.is_some() || permission_revoked(state))
                .map(|state| state.evaluated_revision),
        );
    }
    for resource in active.state.resources() {
        if !is_receiver_resource(resource, kind, publication_id) {
            continue;
        }
        let Some(spec) = active.state.target_spec(resource) else {
            continue;
        };
        let Some(wire::resource_spec::Declaration::Subscription(subscription)) =
            spec.declaration.as_ref()
        else {
            continue;
        };
        if subscription.source_room != selector.room
            || subscription.kind != kind as i32
            || publication_id.is_some_and(|track| subscription.publication_id != track)
        {
            continue;
        }
        let confirmed = active.state.confirmed(resource);
        let evaluation = active.state.evaluation(resource);
        let accepted = confirmed.filter(|state| {
            matches!(
                state.value.as_ref(),
                Some(wire::confirmed_state::Value::Owned(_))
            )
        });
        let rejected = evaluation.filter(|state| {
            state.rejection.is_some()
                || matches!(state.value.as_ref(), Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason() == wire::AbsenceReason::PermissionRevoked)
        });
        return (
            evaluation
                .or(confirmed)
                .map_or(0, |state| state.evaluated_revision),
            accepted.map(|state| state.evaluated_revision),
            rejected.map(|state| state.evaluated_revision),
        );
    }
    (0, None, None)
}

fn publication_revisions(agent: &Agent, track_id: &str) -> (u64, Option<u64>, Option<u64>) {
    let Some(active) = active_agent(agent) else {
        return (0, None, None);
    };
    let resource = Resource::Publication {
        kind: wire::TrackKind::Data,
        track_id: track_id.into(),
    };
    let confirmed = active.state.confirmed(&resource);
    let evaluation = active.state.evaluation(&resource);
    let accepted = confirmed.filter(|state| {
        matches!(
            state.value.as_ref(),
            Some(wire::confirmed_state::Value::Owned(_))
        )
    });
    let rejected = evaluation.filter(|state| {
        state.rejection.is_some()
            || matches!(state.value.as_ref(), Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason() == wire::AbsenceReason::PermissionRevoked)
    });
    (
        evaluation
            .or(confirmed)
            .map_or(0, |state| state.evaluated_revision),
        accepted.map(|state| state.evaluated_revision),
        rejected.map(|state| state.evaluated_revision),
    )
}

fn source_resource(selector: &crate::TrackSelector, kind: wire::TrackKind) -> Resource {
    Resource::SourceLookup {
        room: selector.room.clone(),
        participant_id: selector.participant_id.clone(),
        kind,
        track_id: selector.track_id.clone(),
    }
}

fn source_format(agent: &Agent, publication_id: Option<&str>) -> Option<EffectiveMediaFormat> {
    let track = active_agent(agent)?.state.source(publication_id?)?;
    EffectiveMediaFormat::from_track(track)
}

fn source_state(
    agent: &Agent,
    selector: &crate::TrackSelector,
    kind: wire::TrackKind,
) -> (SubscriptionSourceStatus, Option<String>, Option<ErrorInfo>) {
    let Some(active) = active_agent(agent) else {
        return (SubscriptionSourceStatus::Pending, None, None);
    };
    let legacy = source_resource(selector, kind);
    let resource = super::native::logical_resource(active, selector, kind).unwrap_or(&legacy);
    let Some(evaluation) = active.state.evaluation(resource) else {
        return (SubscriptionSourceStatus::Pending, None, None);
    };
    if let Some(failure) = evaluation.rejection.as_ref() {
        return (
            SubscriptionSourceStatus::Rejected,
            None,
            Some(super::publication::publication_failure(failure)),
        );
    }
    match evaluation.value.as_ref() {
        Some(wire::confirmed_state::Value::LogicalSubscription(state)) => {
            state.source.as_ref().map_or(
                (SubscriptionSourceStatus::Unavailable, None, None),
                |source| {
                    (
                        SubscriptionSourceStatus::Available,
                        Some(source.publication_id.clone()),
                        None,
                    )
                },
            )
        }
        Some(wire::confirmed_state::Value::SourceLookup(lookup)) => {
            let source = lookup.track.as_ref();
            let participant = lookup.participant.as_ref();
            if let (Some(source), Some(participant)) = (source, participant)
                && source.participant_id == participant.participant_id
                && participant.room_id == selector.room
                && participant.participant_id == selector.participant_id
                && source.track_id == selector.track_id
                && Agent::native_track_kind(source) == Some(kind)
            {
                return (
                    SubscriptionSourceStatus::Available,
                    Some(source.publication_id.clone()),
                    None,
                );
            }
            (SubscriptionSourceStatus::Unavailable, None, None)
        }
        Some(wire::confirmed_state::Value::Absent(absent)) => {
            if absent.reason() == wire::AbsenceReason::PermissionRevoked {
                (
                    SubscriptionSourceStatus::Rejected,
                    None,
                    Some(ErrorInfo::new(
                        crate::ffi::ErrorCode::PermissionDenied,
                        "Source subscription permission was revoked",
                    )),
                )
            } else {
                (SubscriptionSourceStatus::Unavailable, None, None)
            }
        }
        _ => (SubscriptionSourceStatus::Unavailable, None, None),
    }
}

fn receiver_outcome(
    agent: &Agent,
    selector: &crate::TrackSelector,
    kind: wire::TrackKind,
    publication_id: Option<&str>,
) -> (
    PublicationAdmissionStatus,
    bool,
    PublicationExecution,
    Option<ErrorInfo>,
) {
    let Some(active) = active_agent(agent) else {
        let failure = agent
            .snapshot()
            .terminal_failure
            .as_ref()
            .map(|failure| ErrorInfo::from(&crate::ffi::Failure::from(failure.clone())));
        return (
            PublicationAdmissionStatus::Pending,
            failure.is_none(),
            if failure.is_some() {
                PublicationExecution::Failed
            } else {
                PublicationExecution::Unavailable
            },
            failure,
        );
    };
    if let Some(resource) = super::native::logical_resource(active, selector, kind) {
        let evaluation = active.state.evaluation(resource);
        if let Some(state) = active
            .state
            .confirmed(resource)
            .and_then(crate::native_state::logical_state)
        {
            let (execution, error) = match &state.execution {
                Some(wire::logical_subscription_state::Execution::Failure(error)) => (
                    PublicationExecution::Failed,
                    Some(super::publication::publication_failure(error)),
                ),
                Some(wire::logical_subscription_state::Execution::Installed(_))
                    if matches!(resource, Resource::LogicalReception(slot)
                        if super::native::receiver_track(active, slot).is_some())
                        || super::native::logical_payload_ready(active, resource) =>
                {
                    (PublicationExecution::Installed, None)
                }
                _ if state
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.settings.as_ref())
                    .is_some_and(|settings| settings.enabled == Some(false)) =>
                {
                    (PublicationExecution::Disabled, None)
                }
                _ => (PublicationExecution::Unavailable, None),
            };
            return (
                PublicationAdmissionStatus::Admitted,
                evaluation.is_none(),
                execution,
                error,
            );
        }
        if evaluation.is_some_and(|state| state.rejection.is_some() || permission_revoked(state)) {
            return (
                PublicationAdmissionStatus::Rejected,
                false,
                PublicationExecution::Unavailable,
                None,
            );
        }
    }
    for resource in active.state.resources() {
        if !is_receiver_resource(resource, kind, publication_id) {
            continue;
        }
        let Some(spec) = active.state.target_spec(resource) else {
            continue;
        };
        let Some(wire::resource_spec::Declaration::Subscription(subscription)) =
            spec.declaration.as_ref()
        else {
            continue;
        };
        if subscription.source_room != selector.room
            || subscription.kind != kind as i32
            || publication_id.is_some_and(|track| subscription.publication_id != track)
        {
            continue;
        }
        let Some(confirmed) = active.state.confirmed(resource) else {
            continue;
        };
        let Some(evaluation) = active.state.evaluation(resource) else {
            if let Some(wire::confirmed_state::Value::Owned(live)) = confirmed.value.as_ref() {
                return (
                    PublicationAdmissionStatus::Admitted,
                    true,
                    execution_status(live.execution()),
                    live.execution_failure
                        .as_ref()
                        .map(super::publication::publication_failure),
                );
            }
            continue;
        };
        if evaluation.rejection.is_some() {
            if let Some(wire::confirmed_state::Value::Owned(live)) = confirmed.value.as_ref() {
                return (
                    PublicationAdmissionStatus::Admitted,
                    false,
                    execution_status(live.execution()),
                    live.execution_failure
                        .as_ref()
                        .map(super::publication::publication_failure),
                );
            }
            return (
                PublicationAdmissionStatus::Rejected,
                false,
                PublicationExecution::Unavailable,
                None,
            );
        }
        if let Some(wire::confirmed_state::Value::Owned(live)) = confirmed.value.as_ref() {
            let execution = execution_status(live.execution());
            let error = live
                .execution_failure
                .as_ref()
                .map(super::publication::publication_failure);
            return (
                PublicationAdmissionStatus::Admitted,
                false,
                execution,
                error,
            );
        }
    }
    (
        PublicationAdmissionStatus::Pending,
        true,
        PublicationExecution::Unavailable,
        None,
    )
}

fn admission_error(
    agent: &Agent,
    selector: &crate::TrackSelector,
    kind: wire::TrackKind,
    publication_id: Option<&str>,
) -> Option<ErrorInfo> {
    let active = active_agent(agent)?;
    if let Some(resource) = super::native::logical_resource(active, selector, kind) {
        return active
            .state
            .evaluation(resource)
            .and_then(|state| state.rejection.as_ref())
            .map(super::publication::publication_failure);
    }
    for resource in active.state.resources() {
        if !is_receiver_resource(resource, kind, publication_id) {
            continue;
        }
        let Some(spec) = active.state.target_spec(resource) else {
            continue;
        };
        let Some(wire::resource_spec::Declaration::Subscription(subscription)) =
            spec.declaration.as_ref()
        else {
            continue;
        };
        if subscription.source_room != selector.room
            || subscription.kind != kind as i32
            || publication_id.is_some_and(|track| subscription.publication_id != track)
        {
            continue;
        }
        if let Some(error) = active
            .state
            .evaluation(resource)
            .and_then(|state| state.rejection.as_ref())
        {
            return Some(super::publication::publication_failure(error));
        }
    }
    None
}

fn publication_outcome(
    agent: &Agent,
    track_id: &str,
) -> (
    PublicationAdmissionStatus,
    bool,
    Option<ErrorInfo>,
    PublicationExecution,
    Option<ErrorInfo>,
) {
    let Some(active) = active_agent(agent) else {
        return (
            PublicationAdmissionStatus::Pending,
            true,
            None,
            PublicationExecution::Unavailable,
            None,
        );
    };
    if agent.resources.data_record_dd(track_id) && !active.quic_data_record_dd {
        return (
            PublicationAdmissionStatus::Rejected,
            false,
            Some(ErrorInfo::new(
                crate::ffi::ErrorCode::Unsupported,
                "The QUIC peer does not support DATA_V1 record descriptors",
            )),
            PublicationExecution::Unavailable,
            None,
        );
    }
    let resource = Resource::Publication {
        kind: wire::TrackKind::Data,
        track_id: track_id.into(),
    };
    let evaluation = active.state.evaluation(&resource);
    let confirmed = active.state.confirmed(&resource);
    let error = evaluation
        .and_then(|state| state.rejection.as_ref())
        .map(super::publication::publication_failure);
    if let Some(wire::confirmed_state::Value::Absent(absent)) =
        evaluation.and_then(|state| state.value.as_ref())
        && absent.reason() == wire::AbsenceReason::PermissionRevoked
    {
        return (
            PublicationAdmissionStatus::Rejected,
            false,
            Some(ErrorInfo::new(
                crate::ffi::ErrorCode::PermissionDenied,
                "Data publication permission was revoked",
            )),
            PublicationExecution::Unavailable,
            None,
        );
    }
    if let Some(wire::confirmed_state::Value::Owned(live)) =
        confirmed.and_then(|state| state.value.as_ref())
    {
        return (
            PublicationAdmissionStatus::Admitted,
            evaluation.is_none(),
            error,
            execution_status(live.execution()),
            live.execution_failure
                .as_ref()
                .map(super::publication::publication_failure),
        );
    }
    (
        if error.is_some() {
            PublicationAdmissionStatus::Rejected
        } else {
            PublicationAdmissionStatus::Pending
        },
        evaluation.is_none(),
        error,
        PublicationExecution::Unavailable,
        None,
    )
}

fn execution_status(status: wire::ExecutionStatus) -> PublicationExecution {
    match status {
        wire::ExecutionStatus::Disabled => PublicationExecution::Disabled,
        wire::ExecutionStatus::Installed => PublicationExecution::Installed,
        wire::ExecutionStatus::Failed => PublicationExecution::Failed,
        wire::ExecutionStatus::WaitingForSource | wire::ExecutionStatus::Unspecified => {
            PublicationExecution::Unavailable
        }
    }
}

fn receive_failure(failure: &crate::TrackResourceError) -> Option<ErrorInfo> {
    match failure {
        crate::TrackResourceError::ReceiveFailed(error) => Some(error.clone()),
        crate::TrackResourceError::ReceiveChannelFailed(_) => Some(ErrorInfo::new(
            crate::ffi::ErrorCode::ConnectionFailed,
            "Data receive channel failed",
        )),
        crate::TrackResourceError::ReceiveOverflow => Some(ErrorInfo::new(
            crate::ffi::ErrorCode::CapacityExceeded,
            "Data receive buffer capacity was exceeded",
        )),
        _ => None,
    }
}

impl Agent {
    fn native_track_kind(track: &wire::Track) -> Option<wire::TrackKind> {
        Some(match track.kind.as_ref()? {
            wire::track::Kind::Audio(_) => wire::TrackKind::Audio,
            wire::track::Kind::Video(_) => wire::TrackKind::Video,
            wire::track::Kind::Data(_) => wire::TrackKind::Data,
        })
    }
}

impl From<&crate::TrackSelector> for crate::ffi::TrackSelector {
    fn from(value: &crate::TrackSelector) -> Self {
        Self {
            room: value.room.clone(),
            participant_id: value.participant_id.clone(),
            track_id: value.track_id.clone(),
        }
    }
}

impl From<&crate::VideoObservation> for crate::ffi::VideoObservation {
    fn from(value: &crate::VideoObservation) -> Self {
        Self {
            visible: value.visible,
            height: value.height,
            min_height: value.min_height,
            min_fps: value.min_fps,
            priority: value.priority,
        }
    }
}
