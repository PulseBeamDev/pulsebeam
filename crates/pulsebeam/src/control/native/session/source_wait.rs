use super::*;
use pulsebeam_runtime::time::Instant;
use std::ops::Bound::{Excluded, Unbounded};

#[derive(Clone)]
pub(super) struct SourceWait {
    pub(super) work: Preparation,
    pub(super) source: PublicationKey,
    pub(super) source_room: String,
    pub(super) deadline: Instant,
}

impl NativeSession {
    pub(in crate::control) fn notify_waiting_source(&mut self, source: PublicationKey) -> bool {
        if !self.source_waits.values().any(|wait| wait.source == source) {
            return false;
        }
        self.source_wait_changed();
        true
    }

    pub(in crate::control) fn source_wait_changed(&mut self) {
        if self.source_waits.is_empty() {
            return;
        }
        if self.source_scan {
            self.source_rescan = true;
        } else {
            self.source_scan = true;
            self.source_cursor = None;
        }
    }

    pub(super) fn retain_source_wait(
        &mut self,
        work: Preparation,
        source: PublicationKey,
        source_room: String,
    ) -> Result<(), OutputError> {
        if self.source_waits.contains_key(&work.reference) {
            return Err(OutputError::InvalidObservation);
        }
        // prepare_receiver prepays this scoped wait and its indexes before returning Waiting.
        let now = Instant::now();
        let deadline = now
            .checked_add(std::time::Duration::from_secs(30))
            .unwrap_or(now);
        self.source_deadlines.insert((deadline, work.reference));
        self.source_waits.insert(
            work.reference,
            SourceWait {
                work,
                source,
                source_room,
                deadline,
            },
        );
        Ok(())
    }

    fn remove_source_wait(&mut self, reference: u64) -> Option<SourceWait> {
        let wait = self.source_waits.remove(&reference)?;
        self.source_deadlines.remove(&(wait.deadline, reference));
        Some(wait)
    }

    pub(in crate::control) fn service_source_waits(
        &mut self,
        unix_now: u64,
        now: Instant,
        available: impl Fn(PublicationKey) -> bool,
    ) -> Result<(), OutputError> {
        self.service_source_waits_in_room(unix_now, now, |_, track| available(track))
    }

    pub(in crate::control) fn service_source_waits_in_room(
        &mut self,
        unix_now: u64,
        now: Instant,
        available: impl Fn(&str, PublicationKey) -> bool,
    ) -> Result<(), OutputError> {
        for _ in 0..16 {
            let Some(&(deadline, reference)) = self.source_deadlines.first() else {
                break;
            };
            if deadline > now {
                break;
            }
            let wait = self
                .remove_source_wait(reference)
                .ok_or(OutputError::InvalidObservation)?;
            self.release_preparation_storage(wait.work, true)?;
            self.complete_projected(
                wait.work,
                Err(failure(wire::FailureCode::InternalFailure)),
                unix_now,
            )?;
        }
        for _ in 0..SELECT_WORK {
            if !self.source_scan {
                break;
            }
            let next = self
                .source_waits
                .range((self.source_cursor.map_or(Unbounded, Excluded), Unbounded))
                .next()
                .map(|(&reference, wait)| (reference, wait.clone()));
            let Some((reference, wait)) = next else {
                self.source_scan = self.source_rescan;
                self.source_rescan = false;
                self.source_cursor = None;
                break;
            };
            self.source_cursor = Some(reference);
            let stale = self.preparation_spec(&wait.work).is_none();
            if stale || available(&wait.source_room, wait.source) {
                self.remove_source_wait(reference);
                self.observe_deferral(super::super::ExecutionDeferral {
                    work: wait.work,
                    retained_sequence: None,
                })?;
            }
        }
        if self.source_waits.is_empty() {
            self.source_scan = false;
            self.source_rescan = false;
            self.source_cursor = None;
        }
        Ok(())
    }
}
