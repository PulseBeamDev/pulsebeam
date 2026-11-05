use super::*;

impl Sender {
    pub fn process_feedback(&mut self, batch: SendFeedback<'_>) {
        if self.inputs.exhausted {
            return;
        }
        if let SendReport::Independent { reports } = batch.report
            && reports.len() > self.config.max_feedback_statuses
        {
            self.add_unknown(reports.len());
            return;
        }
        if self.active_epoch != Some(batch.path_epoch) {
            let count = feedback_status_count(&batch.report);
            self.counters.wrong_path_feedback = self
                .counters
                .wrong_path_feedback
                .saturating_add(count as u64);
            return;
        }
        if !matches!(
            (self.mode, batch.report),
            (
                Some(FeedbackMode::Continuous),
                SendReport::Continuous { .. }
            ) | (
                Some(FeedbackMode::Independent),
                SendReport::Independent { .. }
            )
        ) || feedback_status_count(&batch.report) > self.config.max_feedback_statuses
        {
            self.add_unknown(feedback_status_count(&batch.report));
            return;
        }
        let valid = |status: &SendStatus| {
            status.received
                || (status.arrival.is_none() && status.ecn.is_none() && status.hold.is_none())
        };
        let valid = match batch.report {
            SendReport::Continuous { statuses, .. } => statuses.iter().all(valid),
            SendReport::Independent { reports } => reports
                .iter()
                .all(|report| report.statuses.iter().all(valid)),
        };
        if !valid {
            self.add_unknown(feedback_status_count(&batch.report));
            return;
        }
        let received_at = batch.received_at;
        let prior_evidence = self.inputs.feedback.len();
        let feedback_hold = match batch.report {
            SendReport::Continuous {
                base_sequence,
                reference_micros,
                generation,
                statuses,
                ..
            } => {
                self.process_continuous(
                    base_sequence,
                    reference_micros,
                    generation,
                    statuses,
                    batch.path_epoch,
                    received_at,
                );
                None
            }
            SendReport::Independent { reports } => {
                self.process_independent(reports, batch.path_epoch, received_at)
            }
        };
        self.apply_reordering_window(received_at);
        let newest_send_age = self
            .inputs
            .feedback
            .iter()
            .skip(prior_evidence)
            .filter_map(|feedback| self.entry(feedback.sent_id))
            .map(|entry| received_at.saturating_sub(entry.committed_at))
            .min();
        if self.inputs.feedback.len() > prior_evidence && newest_send_age.is_some() {
            self.inputs.timing = Some(FeedbackTiming {
                received_at,
                feedback_hold,
                newest_send_age,
            });
        }
    }

    pub fn expire(&mut self, now: Duration, limit: usize) -> usize {
        let mut work =
            self.confirm_losses(now, limit.min(self.config.max_expirations_per_poll / 2));
        work += self.reclaim_sources(now, limit.saturating_sub(work).div_ceil(2));
        while work < limit
            && self.loss_work < self.config.max_expirations_per_poll
            && self.oldest_sent_id < self.next_sent_id
        {
            let id = SentPacketId(self.oldest_sent_id);
            let Some(entry) = self.entry(id).copied() else {
                self.oldest_sent_id = self.oldest_sent_id.saturating_add(1);
                work += 1;
                self.loss_work += 1;
                continue;
            };
            let Some(deadline) = entry.committed_at.checked_add(self.config.retention) else {
                break;
            };
            if now < deadline {
                break;
            }
            self.loss_work += 1;
            if !entry.acknowledgment.is_terminal() {
                self.mark_lost_unbudgeted(id, LossOrigin::HistoryExpiry);
                self.counters.expired = self.counters.expired.saturating_add(1);
            }
            self.oldest_sent_id = self.oldest_sent_id.saturating_add(1);
            work += 1;
        }
        work
    }

    pub fn next_deadline(&self, now: Duration) -> Option<Duration> {
        let sent = (self.oldest_sent_id < self.next_sent_id)
            .then(|| self.entry(SentPacketId(self.oldest_sent_id)))
            .flatten()
            .and_then(|entry| entry.committed_at.checked_add(self.config.retention));
        // Variant order partitions the source keys from the deadline-ordered missing keys.
        let missing = self
            .readiness
            .range((
                std::ops::Bound::Excluded(Readiness::Unscheduled(SpaceId(u64::MAX))),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .and_then(|key| match key {
                Readiness::Missing { deadline, .. } => Some(*deadline),
                _ => None,
            });
        let source = match self.readiness.first() {
            Some(Readiness::Retired { deadline, .. }) => Some(deadline.unwrap_or(now)),
            _ => None,
        };
        [sent, missing, source].into_iter().flatten().min()
    }

    fn is_live_feedback_entry(&self, entry: &SentEntry, now: Duration) -> bool {
        !entry.acknowledgment.is_terminal()
            && now.saturating_sub(entry.committed_at) < self.config.retention
            && !self
                .reorder_deadline(entry)
                .is_some_and(|deadline| now >= deadline)
    }

    fn is_recoverable_entry(&self, entry: &SentEntry, now: Duration) -> bool {
        (matches!(entry.acknowledgment, Acknowledgment::Missing { .. })
            || entry.acknowledgment == Acknowledgment::Lost && entry.missing_since.is_some())
            && now.saturating_sub(entry.committed_at) < self.config.retention
    }

    fn recovery_is_fresh(&self, entry: &SentEntry, now: Duration, report_is_new: bool) -> bool {
        self.is_recoverable_entry(entry, now)
            && (report_is_new || self.is_live_feedback_entry(entry, now))
    }

    fn reorder_deadline(&self, entry: &SentEntry) -> Option<Duration> {
        entry.reorder_deadline(self.reordering_window)
    }

    pub(super) fn apply_reordering_window(&mut self, now: Duration) {
        if self.observed_reordering_window <= self.reordering_window {
            return;
        }
        // A window transition visits at most the fixed sent window, once per report.
        // Freeze elapsed deadlines before rekeying, without consuming deferred loss work.
        for entry in self.entries.iter_mut().flatten() {
            let previous = entry.readiness(self.reordering_window);
            if entry.frozen_reorder_deadline.is_none()
                && let Some(deadline) = entry.reorder_deadline(self.reordering_window)
                && deadline <= now
            {
                entry.frozen_reorder_deadline = Some(deadline);
            }
            let next = entry.readiness(self.observed_reordering_window);
            if previous != next {
                if let Some(key) = previous {
                    self.readiness.remove(&key);
                }
                if let Some(key) = next {
                    self.readiness.insert(key);
                }
            }
        }
        self.reordering_window = self.observed_reordering_window;
    }

    fn process_continuous(
        &mut self,
        base_sequence: PacketId,
        reference_micros: i64,
        generation: i64,
        statuses: &[SendStatus],
        epoch: Epoch,
        received_at: Duration,
    ) {
        let (Some(newest), Some(floor)) = (self.newest_feedback, self.oldest_feedback) else {
            self.add_unknown(statuses.len());
            return;
        };
        let base = base_sequence.0;
        if newest.saturating_sub(base) > self.config.max_ack_reordering {
            self.counters.stale_feedback = self
                .counters
                .stale_feedback
                .saturating_add(statuses.len() as u64);
            return;
        }
        let Some(last_reported) = base.checked_add(statuses.len().saturating_sub(1) as u64) else {
            self.add_unknown(statuses.len());
            return;
        };
        if base > newest || last_reported > newest {
            self.add_unknown(statuses.len());
            return;
        }
        if last_reported < floor {
            self.counters.stale_feedback = self
                .counters
                .stale_feedback
                .saturating_add(statuses.len() as u64);
            return;
        }
        let reference = reference_micros;
        let count = generation;
        let arrivals: Vec<_> = statuses.iter().map(|status| status.arrival).collect();
        let old_edge = self.highest_feedback_acked;
        let received_edge = statuses
            .iter()
            .enumerate()
            .rposition(|(offset, status)| {
                base.saturating_add(offset as u64) >= floor && status.received
            })
            .map(|offset| base.saturating_add(offset as u64));
        if !statuses.is_empty() {
            let edge = received_edge.unwrap_or(last_reported);
            let start = old_edge.map_or(floor, |previous| floor.max(previous.saturating_add(1)));
            // A receiver report can straddle a route change. Only the committed
            // current-path suffix can contribute evidence or ACK-edge credit.
            let unknown_reported = (base.max(floor)..=last_reported)
                .filter(|&sequence| {
                    self.feedback_sent_id(sequence)
                        .and_then(|id| self.entry(id))
                        .is_none_or(|entry| entry.path_epoch != epoch)
                })
                .count();
            if received_edge.is_some_and(|edge| {
                edge >= start
                    && edge.saturating_sub(start) >= self.config.max_feedback_statuses as u64
            }) {
                self.add_unknown(statuses.len());
                return;
            }
            let unknown_advance = if received_edge.is_some_and(|edge| edge >= start) {
                (start..base.min(edge.saturating_add(1)))
                    .filter(|&sequence| {
                        self.feedback_sent_id(sequence)
                            .and_then(|id| self.entry(id))
                            .is_none_or(|entry| entry.path_epoch != epoch)
                    })
                    .count()
            } else {
                0
            };
            if unknown_reported + unknown_advance != 0 {
                self.add_unknown(unknown_reported + unknown_advance);
                return;
            }
        }
        let report_is_new = self
            .feedback_generation
            .is_none_or(|previous| count > previous);
        let late_receipt = statuses.iter().enumerate().any(|(offset, status)| {
            base.saturating_add(offset as u64) >= floor
                && status.received
                && self
                    .feedback_sent_id(base.saturating_add(offset as u64))
                    .and_then(|id| self.entry(id))
                    .is_some_and(|entry| self.recovery_is_fresh(entry, received_at, report_is_new))
        });
        let fresh = !statuses.is_empty()
            && (report_is_new || late_receipt)
            && (late_receipt
                || (base.max(floor)..=last_reported).any(|sequence| {
                    self.feedback_sent_id(sequence)
                        .and_then(|id| self.entry(id))
                        .is_some_and(|entry| self.is_live_feedback_entry(entry, received_at))
                }));
        if self
            .feedback_reference
            .is_none_or(|previous| reference >= previous)
        {
            self.feedback_reference = Some(reference);
        }
        if self
            .feedback_generation
            .is_some_and(|previous| count <= previous)
        {
            self.counters.reordered_feedback = self.counters.reordered_feedback.saturating_add(1);
        } else {
            self.feedback_generation = Some(count);
        }
        self.inputs.fresh_network_feedback |= fresh;
        if let Some(edge) = received_edge {
            self.advance_continuous_edge(base, statuses, &arrivals, edge, epoch, received_at);
        }
        for (offset, status) in statuses.iter().enumerate() {
            let sequence = base.saturating_add(offset as u64);
            if sequence < floor {
                continue;
            }
            let sent_id = self.feedback_sent_id(sequence);
            if !status.received {
                if let Some(sent_id) = sent_id {
                    self.set_missing(sent_id, received_at);
                }
            } else if old_edge.is_some_and(|edge| sequence <= edge)
                && !self.reject_replayed_terminal_receipt(
                    sent_id,
                    epoch,
                    received_at,
                    report_is_new,
                )
            {
                self.recover(sent_id, epoch, arrivals[offset], None, received_at);
            }
        }
        if let Some(edge) = received_edge {
            self.anchor_missing(Some(edge), None, received_at);
        }
    }

    fn advance_continuous_edge(
        &mut self,
        base: u64,
        statuses: &[SendStatus],
        arrivals: &[Option<ReceiverTime>],
        edge: u64,
        epoch: Epoch,
        received_at: Duration,
    ) {
        let start = self.highest_feedback_acked.map_or_else(
            || self.oldest_feedback.unwrap_or(base),
            |previous| {
                self.oldest_feedback
                    .unwrap_or(base)
                    .max(previous.saturating_add(1))
            },
        );
        if edge < start || edge.saturating_sub(start) >= self.config.max_feedback_statuses as u64 {
            return;
        }
        let edge_known = self
            .feedback_sent_id(edge)
            .and_then(|sent_id| self.entry(sent_id))
            .is_some_and(|entry| entry.path_epoch == epoch);
        if !edge_known {
            self.add_unknown(1);
            return;
        }
        self.anchor_missing(Some(edge), None, received_at);
        for sequence in start..=edge {
            let sent_id = self.feedback_sent_id(sequence);
            let offset = sequence
                .checked_sub(base)
                .and_then(|value| usize::try_from(value).ok());
            let Some(status) = offset.and_then(|index| statuses.get(index)) else {
                // ACK-edge credit does not turn an unreported gap into loss evidence.
                if let Some(sent_id) = sent_id {
                    self.retire_from_flight(sent_id);
                    self.emit(sent_id, false, true, None, None, None);
                }
                continue;
            };
            let received = status.received;
            let arrival = offset
                .and_then(|index| arrivals.get(index))
                .copied()
                .flatten();
            self.advance_entry(sent_id, epoch, received, arrival, None, received_at);
        }
        self.highest_feedback_acked = Some(edge);
    }

    fn process_independent(
        &mut self,
        reports: &[IndependentReport<'_>],
        epoch: Epoch,
        received_at: Duration,
    ) -> Option<Duration> {
        let mut minimum_hold: Option<Duration> = None;
        for report in reports {
            let Some(state) = self.spaces.get(&report.space) else {
                self.add_unknown(report.statuses.len());
                continue;
            };
            if state.by_sequence.is_empty() {
                self.add_unknown(report.statuses.len());
                continue;
            }
            let space = report.space;
            let floor = state.first_sequence;
            let newest = state.newest_sequence;
            let previous_timestamp = state
                .last_report_generation
                .filter(|(path, _)| *path == epoch)
                .map(|(_, timestamp)| timestamp);
            let old_edge = state.highest_acked_sequence;
            let base = report.base.0;
            if newest.saturating_sub(base) > self.config.max_ack_reordering {
                self.counters.stale_feedback = self
                    .counters
                    .stale_feedback
                    .saturating_add(report.statuses.len() as u64);
                continue;
            }
            let Some(last_reported) =
                base.checked_add(report.statuses.len().saturating_sub(1) as u64)
            else {
                self.add_unknown(report.statuses.len());
                continue;
            };
            if base > newest || last_reported > newest {
                self.add_unknown(report.statuses.len());
                continue;
            }
            if last_reported < floor {
                self.counters.stale_feedback = self
                    .counters
                    .stale_feedback
                    .saturating_add(report.statuses.len() as u64);
                continue;
            }
            let timestamp = report.generation;
            let report_is_new = previous_timestamp.is_none_or(|previous| timestamp > previous);
            let normalized: Vec<_> = report
                .statuses
                .iter()
                .map(|status| (status.received, status.arrival, status.ecn, status.hold))
                .collect();
            let late_receipt = normalized.iter().enumerate().any(|(offset, status)| {
                base.saturating_add(offset as u64) >= floor
                    && status.0
                    && self
                        .lookup_packet(space, base.saturating_add(offset as u64))
                        .and_then(|id| self.entry(id))
                        .is_some_and(|entry| {
                            self.recovery_is_fresh(entry, received_at, report_is_new)
                        })
            });
            let has_nonterminal = !report.statuses.is_empty()
                && (base.max(floor)..=last_reported).any(|sequence| {
                    self.lookup_packet(space, sequence)
                        .and_then(|id| self.entry(id))
                        .is_some_and(|entry| self.is_live_feedback_entry(entry, received_at))
                });
            let received_edge = normalized
                .iter()
                .enumerate()
                .rposition(|(offset, value)| base.saturating_add(offset as u64) >= floor && value.0)
                .map(|offset| base.saturating_add(offset as u64));
            if !report.statuses.is_empty() {
                let edge = received_edge.unwrap_or(last_reported);
                let start =
                    old_edge.map_or(floor, |previous| floor.max(previous.saturating_add(1)));
                if received_edge.is_some_and(|edge| {
                    edge >= start
                        && edge.saturating_sub(start) >= self.config.max_feedback_statuses as u64
                }) {
                    self.add_unknown(report.statuses.len());
                    continue;
                }
                let unresolved_reported = (base.max(floor)..=last_reported)
                    .filter(|&sequence| {
                        self.lookup_packet(space, sequence)
                            .and_then(|id| self.entry(id))
                            .is_none_or(|entry| entry.path_epoch != epoch)
                    })
                    .count();
                let unresolved_advance = if received_edge.is_some_and(|edge| edge >= start) {
                    (start..base.min(edge.saturating_add(1)))
                        .filter(|&sequence| {
                            self.lookup_packet(space, sequence)
                                .and_then(|id| self.entry(id))
                                .is_none_or(|entry| entry.path_epoch != epoch)
                        })
                        .count()
                } else {
                    0
                };
                if unresolved_reported + unresolved_advance != 0 {
                    self.add_unknown(unresolved_reported + unresolved_advance);
                    continue;
                }
                for (offset, (received, _, _, hold)) in normalized.iter().enumerate() {
                    if *received
                        && let Some(hold) = hold
                        && self
                            .lookup_packet(space, base.saturating_add(offset as u64))
                            .and_then(|id| self.entry(id))
                            .is_some_and(|entry| {
                                self.is_live_feedback_entry(entry, received_at)
                                    || self.recovery_is_fresh(entry, received_at, report_is_new)
                            })
                    {
                        minimum_hold = Some(minimum_hold.map_or(*hold, |known| known.min(*hold)));
                    }
                }
                if received_edge.is_some() && edge >= start {
                    self.anchor_missing(None, Some((report.space, edge)), received_at);
                    for sequence in start..=edge {
                        let sent_id = self.lookup_packet(space, sequence);
                        let offset = sequence
                            .checked_sub(base)
                            .and_then(|value| usize::try_from(value).ok());
                        let (received, arrival, ecn, _) = offset
                            .and_then(|index| normalized.get(index))
                            .copied()
                            .unwrap_or((false, None, None, None));
                        self.advance_entry(sent_id, epoch, received, arrival, ecn, received_at);
                    }
                    if let Some(state) = self.spaces.get_mut(&space) {
                        state.highest_acked_sequence = Some(edge);
                    }
                }
            }
            let fresh = (has_nonterminal || late_receipt) && (report_is_new || late_receipt);
            self.inputs.fresh_network_feedback |= fresh;
            if let Some(state) = self.spaces.get_mut(&space)
                && state
                    .last_report_generation
                    .is_none_or(|(path, previous)| path != epoch || timestamp >= previous)
            {
                state.last_report_generation = Some((epoch, timestamp));
            }
            for (offset, (received, arrival, ecn, _)) in normalized.iter().copied().enumerate() {
                let sequence = base.saturating_add(offset as u64);
                if sequence < floor {
                    continue;
                }
                let sent_id = self.lookup_packet(space, sequence);
                if !received {
                    if let Some(sent_id) = sent_id {
                        self.set_missing(sent_id, received_at);
                    }
                } else if old_edge.is_some_and(|edge| sequence <= edge)
                    && !self.reject_replayed_terminal_receipt(
                        sent_id,
                        epoch,
                        received_at,
                        report_is_new,
                    )
                {
                    self.recover(sent_id, epoch, arrival, ecn, received_at);
                }
            }
            if let Some(edge) = received_edge {
                self.anchor_missing(None, Some((report.space, edge)), received_at);
            }
        }
        minimum_hold
    }

    pub(super) fn anchor_missing(
        &mut self,
        feedback_edge: Option<u64>,
        space_edge: Option<(SpaceId, u64)>,
        received_at: Duration,
    ) {
        for id in &self.missing {
            let Some(entry) = self.entries[self.slot(id.0)].as_mut() else {
                continue;
            };
            if entry.id != *id {
                continue;
            }
            let lower = feedback_edge
                .is_some_and(|edge| entry.feedback_sequence.is_some_and(|seq| seq < edge))
                || space_edge.is_some_and(|(space, edge)| {
                    entry.space == space && entry.packet_sequence < edge
                });
            if lower
                && let Acknowledgment::Missing {
                    higher_received_at, ..
                } = &mut entry.acknowledgment
                && higher_received_at.is_none()
            {
                *higher_received_at = Some(received_at);
                if let Some(key) = entry.readiness(self.reordering_window) {
                    self.readiness.insert(key);
                }
            }
        }
    }

    fn advance_entry(
        &mut self,
        sent_id: Option<SentPacketId>,
        epoch: Epoch,
        received: bool,
        receiver_arrival: Option<ReceiverTime>,
        ecn: Option<EcnMark>,
        received_at: Duration,
    ) {
        let Some(sent_id) = sent_id else {
            self.add_unknown(1);
            return;
        };
        let Some(entry) = self.entry(sent_id).copied() else {
            self.add_unknown(1);
            return;
        };
        if entry.path_epoch != epoch {
            self.counters.wrong_path_feedback = self.counters.wrong_path_feedback.saturating_add(1);
            return;
        }
        if received_at.saturating_sub(entry.committed_at) >= self.config.retention {
            self.mark_lost(sent_id, LossOrigin::HistoryExpiry);
            return;
        }
        if !received
            && entry.acknowledgment == Acknowledgment::Pending
            && self.reserve_missing_slot().is_none()
        {
            self.inputs.exhausted = true;
            return;
        }
        self.retire_from_flight(sent_id);
        if received {
            if self
                .reorder_deadline(&entry)
                .is_some_and(|deadline| received_at >= deadline)
                && !self.mark_lost(sent_id, LossOrigin::Reordering)
            {
                return;
            }
            self.set_received(sent_id, true, receiver_arrival, ecn, received_at);
        } else {
            self.set_missing(sent_id, received_at);
            if self.inputs.exhausted {
                return;
            }
            self.emit(sent_id, false, true, None, None, None);
        }
    }

    fn reject_replayed_terminal_receipt(
        &mut self,
        sent_id: Option<SentPacketId>,
        epoch: Epoch,
        received_at: Duration,
        report_is_new: bool,
    ) -> bool {
        if !report_is_new
            && let Some(id) = sent_id
            && let Some(entry) = self.entry(id).copied()
            && entry.path_epoch == epoch
            && received_at.saturating_sub(entry.committed_at) < self.config.retention
            && (entry.acknowledgment == Acknowledgment::Lost
                || self
                    .reorder_deadline(&entry)
                    .is_some_and(|deadline| received_at >= deadline))
        {
            if let Some(since) = self
                .entry_mut(id)
                .and_then(|entry| entry.missing_since.take())
            {
                self.observed_reordering_window = self.observed_reordering_window.max(
                    received_at
                        .saturating_sub(since)
                        .min(self.config.max_reordering_window),
                );
            }
            // Learn bounded reordering without accepting a replay as path evidence.
            if self.mark_lost(id, LossOrigin::Reordering) {
                self.counters.duplicate_feedback =
                    self.counters.duplicate_feedback.saturating_add(1);
            }
            return true;
        }
        false
    }

    pub(super) fn recover(
        &mut self,
        sent_id: Option<SentPacketId>,
        epoch: Epoch,
        receiver_arrival: Option<ReceiverTime>,
        ecn: Option<EcnMark>,
        received_at: Duration,
    ) {
        let Some(sent_id) = sent_id else {
            self.add_unknown(1);
            return;
        };
        let Some(entry) = self.entry(sent_id).copied() else {
            self.add_unknown(1);
            return;
        };
        if entry.path_epoch != epoch {
            self.counters.wrong_path_feedback = self.counters.wrong_path_feedback.saturating_add(1);
            return;
        }
        if received_at.saturating_sub(entry.committed_at) >= self.config.retention {
            if !self.mark_lost(sent_id, LossOrigin::HistoryExpiry) {
                return;
            }
            if let Some(since) = self
                .entry_mut(sent_id)
                .and_then(|entry| entry.missing_since.take())
            {
                self.observed_reordering_window = self.observed_reordering_window.max(
                    received_at
                        .saturating_sub(since)
                        .min(self.config.max_reordering_window),
                );
            }
            return;
        }
        match entry.acknowledgment {
            Acknowledgment::Missing { since, .. } => {
                let observed = received_at
                    .saturating_sub(since)
                    .min(self.config.max_reordering_window);
                let deadline_passed = self
                    .reorder_deadline(&entry)
                    .is_some_and(|deadline| received_at >= deadline);
                if deadline_passed && !self.mark_lost(sent_id, LossOrigin::Reordering) {
                    return;
                }
                self.set_received(sent_id, false, receiver_arrival, ecn, received_at);
                self.observed_reordering_window = self.observed_reordering_window.max(observed);
            }
            Acknowledgment::Lost => {
                if entry.missing_since.is_some() {
                    if let Some(since) = entry.missing_since {
                        self.observed_reordering_window = self.observed_reordering_window.max(
                            received_at
                                .saturating_sub(since)
                                .min(self.config.max_reordering_window),
                        );
                    }
                    self.set_received(sent_id, false, receiver_arrival, ecn, received_at);
                } else {
                    self.counters.duplicate_feedback =
                        self.counters.duplicate_feedback.saturating_add(1);
                }
            }
            Acknowledgment::Received | Acknowledgment::Retired => {
                self.counters.duplicate_feedback =
                    self.counters.duplicate_feedback.saturating_add(1);
            }
            Acknowledgment::Pending => {
                self.retire_from_flight(sent_id);
                self.set_received(sent_id, false, receiver_arrival, ecn, received_at);
            }
        }
    }

    fn set_received(
        &mut self,
        sent_id: SentPacketId,
        newly_acked: bool,
        receiver_arrival: Option<ReceiverTime>,
        ecn: Option<EcnMark>,
        received_at: Duration,
    ) {
        let window = self.reordering_window;
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        if entry.acknowledgment.is_terminal()
            && !(entry.acknowledgment == Acknowledgment::Lost && entry.missing_since.is_some())
        {
            self.counters.duplicate_feedback = self.counters.duplicate_feedback.saturating_add(1);
            return;
        }
        let previous = entry.readiness(window);
        let terminal_loss = entry.acknowledgment == Acknowledgment::Lost;
        if !terminal_loss {
            entry.acknowledgment = Acknowledgment::Received;
        }
        entry.missing_since = None;
        if let Some(key) = previous {
            self.readiness.remove(&key);
        }
        if !terminal_loss {
            self.counters.received = self.counters.received.saturating_add(1);
        }
        self.emit_observed(
            sent_id,
            true,
            newly_acked,
            None,
            receiver_arrival,
            ecn,
            Some(received_at),
        );
    }

    pub(super) fn set_missing(&mut self, sent_id: SentPacketId, now: Duration) {
        if !self
            .entry(sent_id)
            .is_some_and(|entry| entry.acknowledgment == Acknowledgment::Pending)
        {
            return;
        }
        if self.reserve_missing_slot().is_none() {
            self.inputs.exhausted = true;
            return;
        }
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        entry.acknowledgment = Acknowledgment::Missing {
            since: now,
            higher_received_at: None,
        };
        entry.missing_since = Some(now);
        self.missing.push_back(sent_id);
        self.emit_observed(sent_id, false, false, None, None, None, Some(now));
    }

    pub(super) fn confirm_losses(&mut self, now: Duration, limit: usize) -> usize {
        let mut work = 0;
        let scan_limit = limit.min(self.missing.len()).min(
            self.config
                .max_expirations_per_poll
                .saturating_sub(self.loss_work),
        );
        while work < scan_limit {
            let Some(sent_id) = self.missing.pop_front() else {
                break;
            };
            work += 1;
            self.loss_work += 1;
            let Some(entry) = self.entry(sent_id).copied() else {
                continue;
            };
            if matches!(entry.acknowledgment, Acknowledgment::Missing { .. }) {
                let Some(deadline) = self.reorder_deadline(&entry) else {
                    self.missing.push_back(sent_id);
                    continue;
                };
                if now < deadline {
                    self.missing.push_back(sent_id);
                    continue;
                }
                self.mark_lost_unbudgeted(sent_id, LossOrigin::Reordering);
            }
        }
        work
    }

    fn mark_lost(&mut self, sent_id: SentPacketId, origin: LossOrigin) -> bool {
        if self
            .entry(sent_id)
            .is_none_or(|entry| entry.acknowledgment.is_terminal())
        {
            return true;
        }
        if self.loss_work == self.config.max_expirations_per_poll {
            return false;
        }
        self.loss_work += 1;
        self.mark_lost_unbudgeted(sent_id, origin);
        true
    }

    fn mark_lost_unbudgeted(&mut self, sent_id: SentPacketId, mut origin: LossOrigin) {
        if origin == LossOrigin::HistoryExpiry {
            let Some(entry) = self.entry(sent_id).copied() else {
                return;
            };
            self.retire_from_flight(sent_id);
            // Bounded confirmation work can defer an already-due reorder loss.
            if let Some(expiry) = entry.committed_at.checked_add(self.config.retention)
                && self
                    .reorder_deadline(&entry)
                    .is_some_and(|deadline| deadline <= expiry)
            {
                origin = LossOrigin::Reordering;
            }
        }
        let window = self.reordering_window;
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        if entry.acknowledgment.is_terminal() {
            return;
        }
        let previous = entry.readiness(window);
        entry.acknowledgment = Acknowledgment::Lost;
        if let Some(key) = previous {
            self.readiness.remove(&key);
        }
        self.counters.not_received = self.counters.not_received.saturating_add(1);
        self.emit(sent_id, false, false, Some(origin), None, None);
    }

    fn retire_from_flight(&mut self, sent_id: SentPacketId) {
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        if entry.in_flight {
            entry.in_flight = false;
            let wire_len = entry.wire_len as u64;
            self.bytes_in_flight = self.bytes_in_flight.saturating_sub(wire_len);
            self.inputs.bytes_in_flight = self.bytes_in_flight;
        }
    }

    pub(super) fn emit(
        &mut self,
        sent_id: SentPacketId,
        received: bool,
        newly_acked: bool,
        loss_origin: Option<LossOrigin>,
        receiver_arrival: Option<ReceiverTime>,
        ecn: Option<EcnMark>,
    ) {
        self.emit_observed(
            sent_id,
            received,
            newly_acked,
            loss_origin,
            receiver_arrival,
            ecn,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_observed(
        &mut self,
        sent_id: SentPacketId,
        received: bool,
        newly_acked: bool,
        loss_origin: Option<LossOrigin>,
        receiver_arrival: Option<ReceiverTime>,
        ecn: Option<EcnMark>,
        observed_at: Option<Duration>,
    ) {
        if newly_acked
            && !received
            && loss_origin.is_none()
            && let Some(previous) = self.inputs.feedback.last_mut()
            && previous.sent_id == sent_id
            && previous.reported_lost_for_the_first_time
            && !previous.received
        {
            previous.newly_acked = true;
            return;
        }
        if self.entry(sent_id).is_none() {
            return;
        }
        if loss_origin.is_none() && self.reserve_feedback_slot().is_none() {
            self.inputs.exhausted = true;
            return;
        }
        // Independent receipts expose provisional loss/recovery independently
        // of terminal ledger retirement. Span credit alone is not loss evidence.
        let reports_native_loss = self.mode == Some(FeedbackMode::Independent);
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        let reported_lost_for_the_first_time = reports_native_loss
            && !received
            && !entry.network_loss_reported
            && (loss_origin == Some(LossOrigin::Reordering)
                || matches!(entry.acknowledgment, Acknowledgment::Missing { .. }));
        let recovered = received && entry.network_loss_reported;
        if reported_lost_for_the_first_time {
            entry.network_loss_reported = true;
        } else if recovered {
            entry.network_loss_reported = false;
        }
        let sample = SentObservation {
            sent_id,
            space: entry.space,
            committed_at: entry.committed_at,
            transport_bytes: u32::try_from(entry.wire_len).unwrap_or(u32::MAX),
            tag: entry.tag,
            received,
            newly_acked,
            lost: loss_origin.is_some(),
            reported_lost_for_the_first_time,
            recovered,
            observed_at,
            loss_origin,
            receiver_arrival,
            ecn,
        };
        if loss_origin.is_some() {
            if self.inputs.synthetic.len() < self.config.max_expirations_per_poll {
                self.inputs.synthetic.push(sample);
            } else {
                self.inputs.exhausted = true;
            }
        } else {
            self.inputs.feedback.push(sample);
        }
    }

    pub(super) fn feedback_sent_id(&self, sequence: u64) -> Option<SentPacketId> {
        self.feedback_index[self.slot(sequence)]
            .filter(|index| index.sequence == sequence)
            .map(|index| index.sent_id)
    }

    fn lookup_packet(&self, space: SpaceId, sequence: u64) -> Option<SentPacketId> {
        let id = *self.spaces.get(&space)?.by_sequence.get(&sequence)?;
        self.entry(id)
            .filter(|entry| entry.space == space && entry.packet_sequence == sequence)
            .map(|_| id)
    }
}
