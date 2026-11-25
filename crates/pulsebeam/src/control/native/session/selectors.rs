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
                self.selectors
                    .by_reference
                    .insert(reference, Some(Build::default()));
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

    pub(in crate::control) fn selector_scan_pending(&self, unix_now: u64) -> bool {
        self.authorization_current(unix_now)
            && self.selectors.by_reference.values().any(Option::is_some)
    }

    pub(in crate::control) fn refresh_selector(
        &mut self,
        unix_now: u64,
        scan: impl FnOnce(
            &ResourcePermissions,
            SubscriptionSelector,
            Option<NamedTrack>,
        ) -> Option<SelectorPage>,
    ) -> Result<(), OutputError> {
        if !self.selector_scan_pending(unix_now) {
            return Ok(());
        }
        let next = self
            .selectors
            .by_reference
            .range((Excluded(self.selectors.cursor), Unbounded))
            .chain(self.selectors.by_reference.range(..=self.selectors.cursor))
            .find_map(|(&reference, work)| work.as_ref().map(|work| (reference, work.after)));
        let Some((reference, after)) = next else {
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
        let Some(page) = scan(&self.permissions, selector, after) else {
            return self.finish_source_lookup(reference, wire::AbsenceReason::PermissionRevoked);
        };
        let work = self
            .selectors
            .by_reference
            .get_mut(&reference)
            .ok_or(OutputError::InvalidObservation)?;
        let build = work.as_mut().ok_or(OutputError::InvalidObservation)?;
        use wire::SelectorMatchStatus::{Complete, Overflow};
        if build.matches.len().saturating_add(page.sources.len()) > validation::MAX_SELECTOR_MATCHES
        {
            state.status = Overflow as i32;
            state.matches.clear();
        } else {
            build
                .matches
                .extend(
                    page.sources
                        .into_iter()
                        .map(|(name, _)| wire::LogicalSourceAddress {
                            room: name.room.to_string(),
                            participant_id: name.participant.to_string(),
                            kind: match name.kind {
                                crate::entity::TrackKind::Audio => wire::TrackKind::Audio,
                                crate::entity::TrackKind::Video => wire::TrackKind::Video,
                                crate::entity::TrackKind::Data => wire::TrackKind::Data,
                            } as i32,
                            track_id: name.track.to_string(),
                        }),
                );
            if let Some(after) = page.next {
                build.after = Some(after);
                return Ok(());
            }
            state.status = Complete as i32;
            state.matches = std::mem::take(&mut build.matches);
        }
        if build.dirty {
            // A matching name may have appeared behind this build's cursor.
            *build = Build::default();
            return Ok(());
        }
        *work = None;
        self.put(key, value)
    }
}
