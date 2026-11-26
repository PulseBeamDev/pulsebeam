use super::*;
use crate::ffi::{ErrorCode, ErrorInfo};

fn data_subscription_failure(confirmed: &state::ConfirmedState) -> Option<ErrorInfo> {
    if let Some(failure) = &confirmed.rejection {
        let (code, message) = match failure.code() {
            state::FailureCode::PermissionDenied => (
                ErrorCode::PermissionDenied,
                "Source reception is not authorized",
            ),
            state::FailureCode::CapacityExceeded => (
                ErrorCode::CapacityExceeded,
                "Source lookup capacity exceeded",
            ),
            state::FailureCode::Unsupported | state::FailureCode::Incompatible => {
                (ErrorCode::Unsupported, "Source reception is unsupported")
            }
            state::FailureCode::InvalidConfiguration => (
                ErrorCode::InvalidArgument,
                "Invalid source reception request",
            ),
            state::FailureCode::ResourceGone => {
                (ErrorCode::Closed, "Source lookup lifetime has ended")
            }
            state::FailureCode::Unspecified | state::FailureCode::InternalFailure => {
                (ErrorCode::Internal, "Source lookup failed")
            }
        };
        return Some(ErrorInfo::new(code, message));
    }
    matches!(&confirmed.value,
        Some(state::confirmed_state::Value::Absent(absent))
            if absent.reason == state::AbsenceReason::PermissionRevoked as i32)
    .then(|| {
        ErrorInfo::new(
            ErrorCode::PermissionDenied,
            "Source reception permission was revoked",
        )
    })
}

impl Agent {
    pub(super) fn apply_data_read_failure(
        &mut self,
        resource: &Resource,
        confirmed: &state::ConfirmedState,
    ) -> Result<(), AgentError> {
        let Resource::LogicalData(source) = resource else {
            return Ok(());
        };
        let replacement = match &confirmed.value {
            Some(state::confirmed_state::Value::LogicalSubscription(subscription)) => Some(
                subscription
                    .source
                    .as_ref()
                    .map(|track| track.publication_id.as_str()),
            ),
            Some(state::confirmed_state::Value::Absent(absent))
                if matches!(
                    absent.reason(),
                    state::AbsenceReason::SourceEnded
                        | state::AbsenceReason::Gone
                        | state::AbsenceReason::Removed
                ) =>
            {
                Some(None)
            }
            _ => None,
        };
        let source_failure = data_subscription_failure(confirmed);
        let mut retired = replacement.map_or_else(Vec::new, |replacement| {
            self.resources
                .retire_inherited_data_source(source, replacement)
        });
        if let Some(replacement) = replacement.or_else(|| source_failure.as_ref().map(|_| None)) {
            retired.extend(
                self.resources
                    .retire_data_sets_for_source(source, replacement),
            );
        }
        let mut update = if let Some(error) = source_failure {
            self.resources
                .fail_receivers_for_source(source, None, &error)
        } else {
            let delivery = match &confirmed.value {
                Some(state::confirmed_state::Value::LogicalSubscription(subscription)) => {
                    subscription
                        .source
                        .as_ref()
                        .and_then(|track| match &track.kind {
                            Some(state::track::Kind::Data(data)) => {
                                crate::TopicMode::from_delivery(data.delivery)
                            }
                            _ => None,
                        })
                }
                _ => None,
            };
            let error = ErrorInfo::new(
                ErrorCode::InvalidArgument,
                "Data delivery does not match the source",
            );
            let mut update = crate::ReceiveUpdate::default();
            for mode in [
                crate::TopicMode::Latest,
                crate::TopicMode::Lossy,
                crate::TopicMode::Ordered,
            ] {
                if delivery.is_none_or(|delivery| mode == delivery) {
                    continue;
                }
                let rejected = self
                    .resources
                    .fail_receivers_for_source(source, Some(mode), &error);
                update.ready.extend(rejected.ready);
                update.registrations_changed |= rejected.registrations_changed;
            }
            update
        };
        update.ready.extend(retired);
        if update.registrations_changed {
            self.refresh_resources()?;
        }
        if !update.ready.is_empty() {
            self.notifications.push_back(Notification::DataReady {
                owners: update
                    .ready
                    .into_iter()
                    .map(crate::TrackOwner::id)
                    .collect(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
