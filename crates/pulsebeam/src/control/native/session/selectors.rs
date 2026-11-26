use super::*;
use crate::control::{
    lifecycle::SelectorPage, permissions::SubscriptionSelector, topology::NamedTrack,
};

#[derive(Default)]
pub(super) struct Selectors {
    // Disabled and quiescent declarations have no scan in progress.
    by_reference: BTreeMap<u64, Option<Build>>,
    cursor: u64,
}

#[derive(Default)]
struct Build {
    after: Option<NamedTrack>,
    matches: Vec<wire::LogicalSourceAddress>,
    dirty: bool,
    await_reset: bool,
}

pub(super) fn slice(
    value: &wire::ConfirmedState,
    version: u64,
    offset: usize,
) -> (wire::ConfirmedState, Option<usize>) {
    let Some(wire::confirmed_state::Value::SubscriptionSelector(state)) = &value.value else {
        return (value.clone(), None);
    };
    if state.status() == wire::SelectorMatchStatus::Pending {
        return (value.clone(), None);
    }
    let total = state.matches.len();
    let end = (offset + validation::MAX_SELECTOR_SLICE_MATCHES).min(total);
    (
        wire::ConfirmedState {
            evaluated_revision: value.evaluated_revision,
            rejection: value.rejection.clone(),
            value: Some(wire::confirmed_state::Value::SubscriptionSelector(
                Box::new(wire::SubscriptionSelectorState {
                    spec: state.spec.clone(),
                    status: state.status,
                    matches: state.matches[offset..end].to_vec(),
                    membership: Some(wire::SelectorMembershipSpan {
                        revision: version,
                        offset: offset as u32,
                        total: total as u32,
                    }),
                }),
            )),
        },
        (end < total).then_some(end),
    )
}

impl Selectors {
    pub(super) fn full(&self) -> bool {
        self.by_reference.len() >= validation::MAX_SELECTOR_DECLARATIONS
    }

    pub(super) fn remove(&mut self, reference: u64) {
        self.by_reference.remove(&reference);
    }

    pub(super) fn project(
        &mut self,
        reference: u64,
        spec: wire::ResourceSpec,
        previous: Option<&wire::SubscriptionSelectorState>,
    ) -> wire::SubscriptionSelectorState {
        use wire::SelectorMatchStatus::{Complete, Pending};
        let mut state = if enabled(&spec) {
            if let Some(previous) =
                previous.filter(|previous| previous.spec.as_ref().is_some_and(enabled))
            {
                previous.clone()
            } else {
                self.by_reference.insert(reference, Some(Build::default()));
                wire::SubscriptionSelectorState {
                    status: Pending as i32,
                    ..Default::default()
                }
            }
        } else {
            self.by_reference.insert(reference, None);
            wire::SubscriptionSelectorState {
                status: Complete as i32,
                ..Default::default()
            }
        };
        state.spec = Some(spec);
        state
    }
}

impl NativeSession {
    pub(super) fn permits_owned_intent(&self, spec: &wire::ResourceSpec) -> bool {
        match selector_spec(spec) {
            Some(selector) => self
                .connection_identity
                .zip(SubscriptionSelector::from_wire(selector))
                .is_some_and(|((project, _), selector)| {
                    self.permissions.admits_selector(project, selector)
                }),
            None => self.permissions.permits_intent(spec),
        }
    }

    pub(super) fn reset_selector_scans(&mut self) -> Result<(), OutputError> {
        let references: Vec<_> = self.selectors.by_reference.keys().copied().collect();
        for reference in references {
            let key = Key::Owned(reference);
            let mut value = self
                .rows
                .get(&key)
                .ok_or(OutputError::InvalidObservation)?
                .value
                .clone();
            let Some(wire::confirmed_state::Value::SubscriptionSelector(state)) = &mut value.value
            else {
                return Err(OutputError::InvalidObservation);
            };
            if state.spec.as_ref().is_some_and(enabled) {
                state.status = wire::SelectorMatchStatus::Pending as i32;
                state.matches.clear();
                self.selectors.by_reference.insert(
                    reference,
                    Some(Build {
                        await_reset: self.cid.is_some(),
                        ..Default::default()
                    }),
                );
                self.put(key, value)?;
            }
        }
        Ok(())
    }

    pub(in crate::control) fn selector_interests(
        &self,
    ) -> impl Iterator<Item = (u64, SubscriptionSelector)> + '_ {
        self.selectors.by_reference.keys().filter_map(|&reference| {
            let row = self.rows.get(&Key::Owned(reference))?;
            let wire::confirmed_state::Value::SubscriptionSelector(state) =
                row.value.value.as_ref()?
            else {
                return None;
            };
            let spec = state.spec.as_ref()?;
            if self.closed || !enabled(spec) || !self.permits_owned_intent(spec) {
                return None;
            }
            Some((
                reference,
                SubscriptionSelector::from_wire(selector_spec(spec)?)?,
            ))
        })
    }

    pub(in crate::control) fn selector_source_changed(&mut self, reference: u64) {
        if let Some(work) = self.selectors.by_reference.get_mut(&reference) {
            if let Some(build) = work {
                // An event before the first page is covered by that page's newer cut.
                build.dirty |= build.after.is_some();
            } else if self.rows.get(&Key::Owned(reference)).is_some_and(|row| {
                matches!(&row.value.value, Some(wire::confirmed_state::Value::SubscriptionSelector(state)) if state.spec.as_ref().is_some_and(enabled))
            }) {
                *work = Some(Build::default());
            }
        }
    }

    fn next_selector_scan(&self, unix_now: u64) -> Option<(u64, Option<NamedTrack>)> {
        if !self.authorization_current(unix_now) {
            return None;
        }
        self.selectors
            .by_reference
            .range((Excluded(self.selectors.cursor), Unbounded))
            .chain(self.selectors.by_reference.range(..=self.selectors.cursor))
            .find_map(|(&reference, work)| {
                let build = work.as_ref()?;
                // An authority change must withdraw the old cut even when the
                // new scan could finish before the outbox sends another row.
                if build.await_reset
                    && self
                        .rows
                        .get(&Key::Owned(reference))
                        .is_some_and(|row| row.dirty)
                {
                    return None;
                }
                Some((reference, build.after))
            })
    }

    pub(in crate::control) fn selector_scan_pending(&self, unix_now: u64) -> bool {
        self.next_selector_scan(unix_now).is_some()
    }

    pub(in crate::control) fn refresh_selector(
        &mut self,
        unix_now: u64,
        scan: impl FnOnce(
            u64,
            &ResourcePermissions,
            SubscriptionSelector,
            Option<NamedTrack>,
        ) -> Option<SelectorPage>,
    ) -> Result<(), OutputError> {
        let Some((reference, after)) = self.next_selector_scan(unix_now) else {
            return Ok(());
        };
        self.selectors.cursor = reference;
        let key = Key::Owned(reference);
        let mut value = self
            .rows
            .get(&key)
            .ok_or(OutputError::InvalidObservation)?
            .value
            .clone();
        let Some(wire::confirmed_state::Value::SubscriptionSelector(state)) = &mut value.value
        else {
            return Err(OutputError::InvalidObservation);
        };
        let spec = state.spec.as_ref().ok_or(OutputError::InvalidObservation)?;
        if !self.permits_owned_intent(spec) {
            return self.finish_source_lookup(reference, wire::AbsenceReason::PermissionRevoked);
        }
        let selector = selector_spec(spec)
            .and_then(SubscriptionSelector::from_wire)
            .ok_or(OutputError::InvalidObservation)?;
        let Some(page) = scan(reference, &self.permissions, selector, after) else {
            return self.finish_source_lookup(reference, wire::AbsenceReason::PermissionRevoked);
        };
        let work = self
            .selectors
            .by_reference
            .get_mut(&reference)
            .ok_or(OutputError::InvalidObservation)?;
        let build = work.as_mut().ok_or(OutputError::InvalidObservation)?;
        use wire::SelectorMatchStatus::{Complete, Overflow, Pending};
        let remaining = validation::MAX_SELECTOR_MATCHES - build.matches.len();
        let limited = page.sources.len() > remaining;
        build
            .matches
            .extend(page.sources.into_iter().take(remaining).map(|(name, _)| {
                wire::LogicalSourceAddress {
                    room: name.room.to_string(),
                    participant_id: name.participant.to_string(),
                    kind: match name.kind {
                        crate::entity::TrackKind::Audio => wire::TrackKind::Audio,
                        crate::entity::TrackKind::Video => wire::TrackKind::Video,
                        crate::entity::TrackKind::Data => wire::TrackKind::Data,
                    } as i32,
                    track_id: name.track.to_string(),
                }
            }));
        if !limited && let Some(after) = page.next {
            build.after = Some(after);
            return Ok(());
        }
        state.matches = std::mem::take(&mut build.matches);
        state.status = if limited {
            Overflow
        } else if state.matches.is_empty() && !page.empty_confirmed {
            Pending
        } else {
            Complete
        } as i32;
        if build.dirty {
            // A matching name may have appeared behind this build's cursor.
            *build = Build::default();
            return Ok(());
        }
        *work = None;
        self.put(key, value)
    }
}
