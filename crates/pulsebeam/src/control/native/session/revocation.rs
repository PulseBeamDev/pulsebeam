use super::*;
use std::ops::Bound::{Excluded, Included};

#[derive(Clone, Copy)]
pub(super) struct PermissionSweep {
    cursor: u64,
    end: u64,
    again: bool,
}

impl NativeSession {
    pub(super) fn permissions_changed(&mut self) {
        if let Some(sweep) = &mut self.permission_sweep {
            sweep.again = true;
        } else {
            self.permission_sweep = Some(PermissionSweep {
                cursor: 0,
                end: self.ordinal,
                again: false,
            });
        }
    }

    pub(super) fn owned_may_continue(&self, reference: u64) -> bool {
        let Some(row) = self.rows.get(&Key::Owned(reference)) else {
            return false;
        };
        let Some(owned) = &row.owned else {
            return false;
        };
        if !self.permissions.permits_intent(&owned.declaration) {
            return false;
        }
        if matches!(
            owned.declaration.declaration,
            Some(wire::resource_spec::Declaration::Publication(_))
        ) {
            return true;
        }
        if let Some(source) = &owned.receiver_source {
            return self
                .permissions
                .can_subscribe(&source.participant, &source.track);
        }
        match &row.value.value {
            Some(wire::confirmed_state::Value::Owned(live)) => live
                .participant
                .as_ref()
                .zip(live.track.as_ref())
                .is_some_and(|(participant, track)| {
                    self.permissions.can_subscribe(participant, track)
                }),
            // An unresolved intent still needs the full source check before preparation.
            _ => true,
        }
    }

    pub(super) fn permission_denial_reason(&self, reference: u64) -> wire::AbsenceReason {
        if self.rows.get(&Key::Owned(reference)).is_some_and(|row| {
            matches!(
                row.value.value,
                Some(wire::confirmed_state::Value::Owned(_))
            )
        }) {
            wire::AbsenceReason::PermissionRevoked
        } else {
            wire::AbsenceReason::CreateRejected
        }
    }

    pub(super) fn revoke(
        &mut self,
        reference: u64,
        reason: wire::AbsenceReason,
    ) -> Result<(), OutputError> {
        let owned = self
            .rows
            .get_mut(&Key::Owned(reference))
            .and_then(|row| row.owned.as_mut())
            .ok_or(OutputError::InvalidObservation)?;
        owned.retired = true;
        owned.cutoff_pending = true;
        owned.cutoff_reason = reason;
        Ok(())
    }

    pub(in crate::control) fn revocation_pending(&self) -> bool {
        self.permission_sweep.is_some()
    }

    pub(in crate::control) fn service_revocations<'a>(
        &mut self,
        capacity: usize,
        source: impl Fn(TrackId) -> Option<super::super::ResolvedSource<'a>>,
    ) -> Result<Vec<ExecutionCommand>, OutputError> {
        let mut commands = Vec::new();
        let Some(mut sweep) = self.permission_sweep else {
            return Ok(commands);
        };
        for _ in 0..SELECT_WORK {
            let next = self
                .order
                .range((Excluded(sweep.cursor), Included(sweep.end)))
                .next()
                .map(|(&ordinal, key)| (ordinal, key.clone()));
            let Some((ordinal, key)) = next else {
                self.permission_sweep = sweep.again.then_some(PermissionSweep {
                    cursor: 0,
                    end: self.ordinal,
                    again: false,
                });
                if self.permission_sweep.is_none() {
                    self.discovery_reconciling = false;
                }
                return Ok(commands);
            };
            sweep.cursor = ordinal;
            if let Key::Owned(reference) = key
                && self
                    .rows
                    .get(&key)
                    .and_then(|row| row.owned.as_ref())
                    .is_some_and(|owned| !owned.retired)
                && !self.owned_may_continue(reference)
            {
                if commands.len() < capacity {
                    self.revoke(reference, wire::AbsenceReason::PermissionRevoked)?;
                    commands.push(self.retirement_command(reference)?);
                } else {
                    sweep.again = true;
                }
            }
            if self.discovery_reconciling {
                self.reconcile_discovery(&key, &source)?;
            }
        }
        self.permission_sweep = Some(sweep);
        Ok(commands)
    }
}
