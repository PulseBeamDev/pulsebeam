use super::{NativeSession, wire};
use crate::{
    entity::{ConnectionId, ProjectId, PublicationId, TrackKind},
    log::subscription_event,
};

impl NativeSession {
    pub(in crate::control) fn identify(&mut self, project: ProjectId, connection: ConnectionId) {
        self.incident_context = Some((project, connection));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Bound(PublicationId),
    Waiting(&'static str),
}

fn subscription(value: &wire::ConfirmedState) -> Option<(TrackKind, Phase)> {
    use wire::{confirmed_state::Value, logical_subscription_state::Execution};
    let (kind, phase) = match value.value.as_ref()? {
        Value::Owned(live) => {
            let wire::resource_spec::Declaration::Subscription(spec) =
                live.spec.as_ref()?.declaration.as_ref()?
            else {
                return None;
            };
            let phase = match live.execution() {
                wire::ExecutionStatus::Installed => Phase::Bound(spec.publication_id.parse().ok()?),
                wire::ExecutionStatus::WaitingForSource => Phase::Waiting("source_unavailable"),
                wire::ExecutionStatus::Failed => Phase::Waiting("execution_failed"),
                wire::ExecutionStatus::Disabled => Phase::Waiting("disabled"),
                _ => return None,
            };
            (spec.kind(), phase)
        }
        Value::LogicalSubscription(logical) => {
            let spec = logical.spec.as_ref()?;
            let wire::resource_spec::Declaration::LogicalSubscription(declaration) =
                spec.declaration.as_ref()?
            else {
                return None;
            };
            let phase = match &logical.execution {
                Some(Execution::Installed(_)) => {
                    Phase::Bound(logical.source.as_ref()?.publication_id.parse().ok()?)
                }
                Some(Execution::Failure(_)) => Phase::Waiting("execution_failed"),
                None if !spec.settings.as_ref()?.enabled.unwrap_or(false) => {
                    Phase::Waiting("disabled")
                }
                None if logical.source.is_some() => Phase::Waiting("preparing"),
                None => Phase::Waiting("source_unavailable"),
            };
            (declaration.source.as_ref()?.kind(), phase)
        }
        _ => return None,
    };
    let kind = match kind {
        wire::TrackKind::Audio => TrackKind::Audio,
        wire::TrackKind::Video => TrackKind::Video,
        wire::TrackKind::Data => TrackKind::Data,
        _ => return None,
    };
    Some((kind, phase))
}

pub(super) fn subscription_changed(
    context: (ProjectId, ConnectionId),
    owned: u64,
    previous: Option<&wire::ConfirmedState>,
    next: &wire::ConfirmedState,
) {
    let previous = previous.and_then(subscription);
    let current = subscription(next);
    if current == previous {
        return;
    }
    let Some((kind, phase)) = current else {
        if let (Some((kind, _)), Some(wire::confirmed_state::Value::Absent(absent))) =
            (previous, &next.value)
        {
            let reason = match absent.reason() {
                wire::AbsenceReason::Removed => "removed",
                wire::AbsenceReason::SourceEnded => "source_ended",
                wire::AbsenceReason::PermissionRevoked => "permission_revoked",
                _ => "gone",
            };
            subscription_event(
                context,
                owned,
                kind,
                "subscription_retired",
                None,
                Some(reason),
            );
        }
        return;
    };
    if previous.is_none() {
        subscription_event(context, owned, kind, "subscription_admitted", None, None);
    }
    match phase {
        Phase::Bound(publication) => {
            let event = if matches!(previous, Some((_, Phase::Bound(_)))) {
                "subscription_replaced"
            } else {
                "subscription_bound"
            };
            subscription_event(context, owned, kind, event, Some(publication), None);
        }
        Phase::Waiting(reason) => {
            subscription_event(
                context,
                owned,
                kind,
                "subscription_waiting",
                None,
                Some(reason),
            );
        }
    }
}
