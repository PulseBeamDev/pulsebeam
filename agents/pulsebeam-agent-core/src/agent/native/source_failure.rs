use super::*;
use crate::ffi::{ErrorCode, ErrorInfo};

fn lookup_failure(confirmed: &state::ConfirmedState) -> Option<ErrorInfo> {
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
    pub(super) fn apply_lookup_read_failure(
        &mut self,
        resource: &Resource,
        confirmed: &state::ConfirmedState,
    ) -> Result<(), AgentError> {
        let Resource::SourceLookup {
            room,
            participant_id,
            kind: state::TrackKind::Data,
            track_id,
        } = resource
        else {
            return Ok(());
        };
        let source = crate::TrackSelector {
            room: room.clone(),
            participant_id: participant_id.clone(),
            track_id: track_id.clone(),
        };
        let update = if let Some(error) = lookup_failure(confirmed) {
            self.resources
                .fail_receivers_for_source(&source, None, &error)
        } else {
            let Some(state::confirmed_state::Value::SourceLookup(lookup)) = &confirmed.value else {
                return Ok(());
            };
            let Some(state::track::Kind::Data(data)) =
                lookup.track.as_ref().and_then(|track| track.kind.as_ref())
            else {
                return Ok(());
            };
            let Some(delivery) = crate::TopicMode::from_delivery(data.delivery) else {
                return Ok(());
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
                if mode == delivery {
                    continue;
                }
                let rejected =
                    self.resources
                        .fail_receivers_for_source(&source, Some(mode), &error);
                update.ready.extend(rejected.ready);
                update.registrations_changed |= rejected.registrations_changed;
            }
            update
        };
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
