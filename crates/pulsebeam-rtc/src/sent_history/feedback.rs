use super::*;

impl SentHistory {
    pub(crate) fn process_feedback(&mut self, batch: FeedbackBatch) {
        if self.inputs.exhausted {
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
        if feedback_status_count(&batch.report) > MAX_FEEDBACK_STATUSES {
            self.add_unknown(feedback_status_count(&batch.report));
            return;
        }
        let received_at = batch.received_at;
        let prior_evidence = self.inputs.feedback.len();
        let feedback_hold = match batch.report {
            FeedbackReport::Twcc {
                base_sequence,
                reference_time,
                feedback_count,
                statuses,
                ..
            } => {
                self.process_twcc(
                    base_sequence,
                    reference_time,
                    feedback_count,
                    &statuses,
                    batch.path_epoch,
                    received_at,
                );
                None
            }
            FeedbackReport::Rfc8888 {
                reports,
                report_timestamp,
            } => self.process_rfc8888(&reports, report_timestamp, batch.path_epoch, received_at),
        };
        self.apply_reordering_window(received_at);
        let newest_send_age = self
            .inputs
            .feedback
            .iter()
            .skip(prior_evidence)
            .filter_map(|feedback| self.entry(feedback.sent_id))
            .map(|entry| received_at.saturating_duration_since(entry.committed_at))
            .min();
        if self.inputs.feedback.len() > prior_evidence && newest_send_age.is_some() {
            self.inputs.timing = Some(FeedbackTiming {
                received_at,
                feedback_hold,
                newest_send_age,
            });
        }
    }

    pub(crate) fn expire(&mut self, now: Instant, limit: usize) -> usize {
        let mut work = self.confirm_losses(now, limit.min(MAX_EXPIRATIONS_PER_POLL / 2));
        while work < limit
            && self.loss_work < MAX_EXPIRATIONS_PER_POLL
            && self.oldest_sent_id < self.next_sent_id
        {
            let id = SentPacketId(self.oldest_sent_id);
            let Some(entry) = self.entry(id).copied() else {
                self.oldest_sent_id = self.oldest_sent_id.saturating_add(1);
                work += 1;
                self.loss_work += 1;
                continue;
            };
            let Some(deadline) = entry.committed_at.checked_add(SENT_HISTORY_MAX_AGE) else {
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

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let scan_end = self
            .oldest_sent_id
            .saturating_add(SENT_HISTORY_CAPACITY as u64)
            .min(self.next_sent_id);
        let sent = (self.oldest_sent_id..scan_end).find_map(|id| {
            self.entry(SentPacketId(id))
                .and_then(|entry| entry.committed_at.checked_add(SENT_HISTORY_MAX_AGE))
        });
        let missing = self
            .missing
            .iter()
            .filter_map(|id| {
                let entry = self.entry(*id)?;
                self.reorder_deadline(entry)
            })
            .min();
        match (sent, missing) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, None) | (None, left) => left,
        }
    }

    fn is_live_feedback_entry(&self, entry: &SentEntry, now: Instant) -> bool {
        !entry.acknowledgment.is_terminal()
            && now.saturating_duration_since(entry.committed_at) < SENT_HISTORY_MAX_AGE
            && !self
                .reorder_deadline(entry)
                .is_some_and(|deadline| now >= deadline)
    }

    fn is_recoverable_entry(&self, entry: &SentEntry, now: Instant) -> bool {
        (matches!(entry.acknowledgment, Acknowledgment::Missing { .. })
            || entry.acknowledgment == Acknowledgment::Lost && entry.missing_since.is_some())
            && now.saturating_duration_since(entry.committed_at) < SENT_HISTORY_MAX_AGE
    }

    fn recovery_is_fresh(&self, entry: &SentEntry, now: Instant, report_is_new: bool) -> bool {
        self.is_recoverable_entry(entry, now)
            && (report_is_new || self.is_live_feedback_entry(entry, now))
    }

    fn reorder_deadline(&self, entry: &SentEntry) -> Option<Instant> {
        match entry.acknowledgment {
            Acknowledgment::Missing {
                higher_received_at: Some(anchor),
                ..
            } => entry
                .frozen_reorder_deadline
                .or_else(|| anchor.checked_add(self.reordering_window)),
            _ => None,
        }
    }

    pub(super) fn apply_reordering_window(&mut self, now: Instant) {
        if self.observed_reordering_window <= self.reordering_window {
            return;
        }
        // Once per report, preserve elapsed deadlines without consuming deferred loss work.
        for entry in self.entries.iter_mut().flatten() {
            if entry.frozen_reorder_deadline.is_none()
                && let Acknowledgment::Missing {
                    higher_received_at: Some(anchor),
                    ..
                } = entry.acknowledgment
                && let Some(deadline) = anchor.checked_add(self.reordering_window)
                && deadline <= now
            {
                entry.frozen_reorder_deadline = Some(deadline);
            }
        }
        self.reordering_window = self.observed_reordering_window;
    }

    fn process_twcc(
        &mut self,
        base_sequence: u16,
        reference_time: u32,
        feedback_count: u8,
        statuses: &[TwccStatus],
        epoch: PathEpoch,
        received_at: Instant,
    ) {
        let (Some(newest), Some(floor)) = (self.newest_twcc, self.oldest_twcc) else {
            self.add_unknown(statuses.len());
            return;
        };
        let Some(base) = unwrap_near(u64::from(base_sequence), newest, 1 << 16) else {
            self.add_unknown(statuses.len());
            return;
        };
        if newest.saturating_sub(base) > MAX_ACK_REORDERING {
            self.counters.stale_feedback = self
                .counters
                .stale_feedback
                .saturating_add(statuses.len() as u64);
            return;
        }
        let last_reported = base
            .saturating_add(u64::try_from(statuses.len().saturating_sub(1)).unwrap_or(u64::MAX));
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
        let reference = unwrap_near_signed(i64::from(reference_time), self.twcc_reference, 1 << 24);
        let count = unwrap_near_signed(i64::from(feedback_count), self.twcc_feedback_count, 1 << 8);
        let mut arrivals = Vec::with_capacity(statuses.len());
        let mut arrival_micros = reference.saturating_mul(64_000);
        for status in statuses {
            let arrival = match status {
                TwccStatus::NotReceived => None,
                TwccStatus::Received { delta_250us } => {
                    arrival_micros =
                        arrival_micros.saturating_add(i64::from(*delta_250us).saturating_mul(250));
                    Some(ReceiverTime::Micros(arrival_micros))
                }
            };
            arrivals.push(arrival);
        }
        let old_edge = self.highest_twcc_acked;
        let received_edge = statuses
            .iter()
            .enumerate()
            .rposition(|(offset, status)| {
                base.saturating_add(offset as u64) >= floor
                    && matches!(status, TwccStatus::Received { .. })
            })
            .map(|offset| base.saturating_add(offset as u64));
        if !statuses.is_empty() {
            let edge = received_edge.unwrap_or(last_reported);
            let start = old_edge.map_or(floor, |previous| floor.max(previous.saturating_add(1)));
            // A receiver report can straddle a route change. Only the committed
            // current-path suffix can contribute evidence or ACK-edge credit.
            let unknown_reported = (base.max(floor)..=last_reported)
                .filter(|&sequence| {
                    self.twcc_sent_id(sequence)
                        .and_then(|id| self.entry(id))
                        .is_none_or(|entry| entry.path_epoch != epoch)
                })
                .count();
            if received_edge.is_some_and(|edge| {
                edge >= start && edge.saturating_sub(start) >= MAX_FEEDBACK_STATUSES as u64
            }) {
                self.add_unknown(statuses.len());
                return;
            }
            let unknown_advance = if received_edge.is_some_and(|edge| edge >= start) {
                (start..base.min(edge.saturating_add(1)))
                    .filter(|&sequence| {
                        self.twcc_sent_id(sequence)
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
            .twcc_feedback_count
            .is_none_or(|previous| count > previous);
        let late_receipt = statuses.iter().enumerate().any(|(offset, status)| {
            base.saturating_add(offset as u64) >= floor
                && matches!(status, TwccStatus::Received { .. })
                && self
                    .twcc_sent_id(base.saturating_add(offset as u64))
                    .and_then(|id| self.entry(id))
                    .is_some_and(|entry| self.recovery_is_fresh(entry, received_at, report_is_new))
        });
        let fresh = !statuses.is_empty()
            && (report_is_new || late_receipt)
            && (late_receipt
                || (base.max(floor)..=last_reported).any(|sequence| {
                    self.twcc_sent_id(sequence)
                        .and_then(|id| self.entry(id))
                        .is_some_and(|entry| self.is_live_feedback_entry(entry, received_at))
                }));
        if self
            .twcc_reference
            .is_none_or(|previous| reference >= previous)
        {
            self.twcc_reference = Some(reference);
        }
        if self
            .twcc_feedback_count
            .is_some_and(|previous| count <= previous)
        {
            self.counters.reordered_feedback = self.counters.reordered_feedback.saturating_add(1);
        } else {
            self.twcc_feedback_count = Some(count);
        }
        self.inputs.fresh_network_feedback |= fresh;
        if let Some(edge) = received_edge {
            self.advance_twcc_edge(base, statuses, &arrivals, edge, epoch, received_at);
        }
        for (offset, status) in statuses.iter().enumerate() {
            let sequence = base.saturating_add(offset as u64);
            if sequence < floor {
                continue;
            }
            let sent_id = self.twcc_sent_id(sequence);
            if matches!(status, TwccStatus::NotReceived) {
                if let Some(sent_id) = sent_id {
                    self.set_missing(sent_id, received_at);
                }
            } else if old_edge.is_some_and(|edge| sequence <= edge) {
                if !self.reject_replayed_terminal_receipt(
                    sent_id,
                    epoch,
                    received_at,
                    report_is_new,
                ) {
                    self.recover(sent_id, epoch, arrivals[offset], None, received_at);
                }
            }
        }
        if let Some(edge) = received_edge {
            self.anchor_missing(Some(edge), None, received_at);
        }
    }

    fn validate_twcc_range(&self, start: u64, edge: u64, epoch: PathEpoch) -> bool {
        edge.saturating_sub(start) < MAX_FEEDBACK_STATUSES as u64
            && (start..=edge).all(|sequence| {
                self.twcc_sent_id(sequence)
                    .and_then(|id| self.entry(id))
                    .is_some_and(|entry| entry.path_epoch == epoch)
            })
    }

    fn advance_twcc_edge(
        &mut self,
        base: u64,
        statuses: &[TwccStatus],
        arrivals: &[Option<ReceiverTime>],
        edge: u64,
        epoch: PathEpoch,
        received_at: Instant,
    ) {
        let start = self.highest_twcc_acked.map_or_else(
            || self.oldest_twcc.unwrap_or(base),
            |previous| {
                self.oldest_twcc
                    .unwrap_or(base)
                    .max(previous.saturating_add(1))
            },
        );
        if edge < start || edge.saturating_sub(start) >= MAX_FEEDBACK_STATUSES as u64 {
            return;
        }
        let edge_known = self
            .twcc_sent_id(edge)
            .and_then(|sent_id| self.entry(sent_id))
            .is_some_and(|entry| entry.path_epoch == epoch);
        if !edge_known {
            self.add_unknown(1);
            return;
        }
        self.anchor_missing(Some(edge), None, received_at);
        for sequence in start..=edge {
            let sent_id = self.twcc_sent_id(sequence);
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
            let received = matches!(status, TwccStatus::Received { .. });
            let arrival = offset
                .and_then(|index| arrivals.get(index))
                .copied()
                .flatten();
            self.advance_entry(sent_id, epoch, received, arrival, None, received_at);
        }
        self.highest_twcc_acked = Some(edge);
    }

    fn process_rfc8888(
        &mut self,
        reports: &[crate::rtcp::Rfc8888Report],
        report_timestamp: u32,
        epoch: PathEpoch,
        received_at: Instant,
    ) -> Option<Duration> {
        let mut minimum_hold: Option<Duration> = None;
        for report in reports {
            let Some(ssrc_index) = self
                .ssrcs
                .iter()
                .position(|state| state.ssrc == report.ssrc)
            else {
                self.add_unknown(report.statuses.len());
                continue;
            };
            if self.ssrcs[ssrc_index].sent_ids.is_empty() {
                self.add_unknown(report.statuses.len());
                continue;
            }
            let floor = self.ssrcs[ssrc_index].first_sequence;
            let newest = self.ssrcs[ssrc_index].newest_sequence;
            let Some(base) = unwrap_near(u64::from(report.begin_sequence), newest, 1 << 16) else {
                self.add_unknown(report.statuses.len());
                continue;
            };
            if newest.saturating_sub(base) > MAX_ACK_REORDERING {
                self.counters.stale_feedback = self
                    .counters
                    .stale_feedback
                    .saturating_add(report.statuses.len() as u64);
                continue;
            }
            let last_reported = base.saturating_add(
                u64::try_from(report.statuses.len().saturating_sub(1)).unwrap_or(u64::MAX),
            );
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
            let timestamp = unwrap_near_signed(
                i64::from(report_timestamp),
                self.ssrcs[ssrc_index].last_report_timestamp,
                1_i64 << 32,
            );
            let report_is_new = self.ssrcs[ssrc_index]
                .last_report_timestamp
                .is_none_or(|previous| timestamp > previous);
            let report_micros = timestamp.saturating_mul(1_000_000) / 65_536;
            let mut normalized = Vec::with_capacity(report.statuses.len());
            for status in &report.statuses {
                let value = match status {
                    Rfc8888Status::NotReceived => (false, None, None, None),
                    Rfc8888Status::Received {
                        ecn,
                        arrival_offset,
                    } => match arrival_offset {
                        ArrivalOffset::Ticks(ticks) => {
                            let hold_micros = u64::from(*ticks).saturating_mul(1_000_000) / 1_024;
                            (
                                true,
                                Some(ReceiverTime::Micros(
                                    report_micros.saturating_sub(hold_micros as i64),
                                )),
                                Some(ecn_mark(*ecn)),
                                Some(Duration::from_micros(hold_micros)),
                            )
                        }
                        ArrivalOffset::OverRange => (
                            true,
                            Some(ReceiverTime::OverRange),
                            Some(ecn_mark(*ecn)),
                            None,
                        ),
                        ArrivalOffset::Unavailable => (
                            true,
                            Some(ReceiverTime::Unavailable),
                            Some(ecn_mark(*ecn)),
                            None,
                        ),
                    },
                };
                normalized.push(value);
            }
            let late_receipt = normalized.iter().enumerate().any(|(offset, status)| {
                base.saturating_add(offset as u64) >= floor
                    && status.0
                    && self
                        .lookup_rtp(ssrc_index, base.saturating_add(offset as u64))
                        .and_then(|id| self.entry(id))
                        .is_some_and(|entry| {
                            self.recovery_is_fresh(entry, received_at, report_is_new)
                        })
            });
            let has_nonterminal = !report.statuses.is_empty()
                && (base.max(floor)..=last_reported).any(|sequence| {
                    self.lookup_rtp(ssrc_index, sequence)
                        .and_then(|id| self.entry(id))
                        .is_some_and(|entry| self.is_live_feedback_entry(entry, received_at))
                });
            let old_edge = self.ssrcs[ssrc_index].highest_acked_sequence;
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
                    edge >= start && edge.saturating_sub(start) >= MAX_FEEDBACK_STATUSES as u64
                }) {
                    self.add_unknown(report.statuses.len());
                    continue;
                }
                let unresolved_reported = (base.max(floor)..=last_reported)
                    .filter(|&sequence| {
                        self.lookup_rtp(ssrc_index, sequence)
                            .and_then(|id| self.entry(id))
                            .is_none_or(|entry| entry.path_epoch != epoch)
                    })
                    .count();
                let unresolved_advance = if received_edge.is_some_and(|edge| edge >= start) {
                    (start..base.min(edge.saturating_add(1)))
                        .filter(|&sequence| {
                            self.lookup_rtp(ssrc_index, sequence)
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
                            .lookup_rtp(ssrc_index, base.saturating_add(offset as u64))
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
                    self.anchor_missing(None, Some((report.ssrc, edge)), received_at);
                    for sequence in start..=edge {
                        let sent_id = self.lookup_rtp(ssrc_index, sequence);
                        let offset = sequence
                            .checked_sub(base)
                            .and_then(|value| usize::try_from(value).ok());
                        let (received, arrival, ecn, _) = offset
                            .and_then(|index| normalized.get(index))
                            .copied()
                            .unwrap_or((false, None, None, None));
                        self.advance_entry(sent_id, epoch, received, arrival, ecn, received_at);
                    }
                    self.ssrcs[ssrc_index].highest_acked_sequence = Some(edge);
                }
            }
            let fresh = (has_nonterminal || late_receipt) && (report_is_new || late_receipt);
            self.inputs.fresh_network_feedback |= fresh;
            if self.ssrcs[ssrc_index]
                .last_report_timestamp
                .is_none_or(|previous| timestamp >= previous)
            {
                self.ssrcs[ssrc_index].last_report_timestamp = Some(timestamp);
            }
            for (offset, (received, arrival, ecn, _)) in normalized.iter().copied().enumerate() {
                let sequence = base.saturating_add(offset as u64);
                if sequence < floor {
                    continue;
                }
                let sent_id = self.lookup_rtp(ssrc_index, sequence);
                if !received {
                    if let Some(sent_id) = sent_id {
                        self.set_missing(sent_id, received_at);
                    }
                } else if old_edge.is_some_and(|edge| sequence <= edge) {
                    if !self.reject_replayed_terminal_receipt(
                        sent_id,
                        epoch,
                        received_at,
                        report_is_new,
                    ) {
                        self.recover(sent_id, epoch, arrival, ecn, received_at);
                    }
                }
            }
            if let Some(edge) = received_edge {
                self.anchor_missing(None, Some((report.ssrc, edge)), received_at);
            }
        }
        minimum_hold
    }

    pub(super) fn anchor_missing(
        &mut self,
        twcc_edge: Option<u64>,
        rtp_edge: Option<(u32, u64)>,
        received_at: Instant,
    ) {
        for id in &self.missing {
            let Some(entry) = self.entries[ring_index(id.0)].as_mut() else {
                continue;
            };
            if entry.id != *id {
                continue;
            }
            let lower = twcc_edge
                .is_some_and(|edge| entry.twcc_sequence.is_some_and(|seq| seq < edge))
                || rtp_edge
                    .is_some_and(|(ssrc, edge)| entry.ssrc == ssrc && entry.rtp_sequence < edge);
            if lower
                && let Acknowledgment::Missing {
                    higher_received_at, ..
                } = &mut entry.acknowledgment
            {
                higher_received_at.get_or_insert(received_at);
            }
        }
    }

    fn advance_entry(
        &mut self,
        sent_id: Option<SentPacketId>,
        epoch: PathEpoch,
        received: bool,
        receiver_arrival: Option<ReceiverTime>,
        ecn: Option<EcnMark>,
        received_at: Instant,
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
        if received_at.saturating_duration_since(entry.committed_at) >= SENT_HISTORY_MAX_AGE {
            self.mark_lost(sent_id, LossOrigin::HistoryExpiry);
            return;
        }
        self.retire_from_flight(sent_id);
        if received {
            if self
                .reorder_deadline(&entry)
                .is_some_and(|deadline| received_at >= deadline)
            {
                if !self.mark_lost(sent_id, LossOrigin::Reordering) {
                    return;
                }
            }
            self.set_received(sent_id, true, receiver_arrival, ecn, received_at);
        } else {
            self.set_missing(sent_id, received_at);
            self.emit(sent_id, false, true, None, None, None);
        }
    }

    fn reject_replayed_terminal_receipt(
        &mut self,
        sent_id: Option<SentPacketId>,
        epoch: PathEpoch,
        received_at: Instant,
        report_is_new: bool,
    ) -> bool {
        if !report_is_new
            && let Some(id) = sent_id
            && let Some(entry) = self.entry(id).copied()
            && entry.path_epoch == epoch
            && received_at.saturating_duration_since(entry.committed_at) < SENT_HISTORY_MAX_AGE
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
                        .saturating_duration_since(since)
                        .min(MAX_REORDERING_WINDOW),
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
        epoch: PathEpoch,
        receiver_arrival: Option<ReceiverTime>,
        ecn: Option<EcnMark>,
        received_at: Instant,
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
        if received_at.saturating_duration_since(entry.committed_at) >= SENT_HISTORY_MAX_AGE {
            if !self.mark_lost(sent_id, LossOrigin::HistoryExpiry) {
                return;
            }
            if let Some(since) = self
                .entry_mut(sent_id)
                .and_then(|entry| entry.missing_since.take())
            {
                self.observed_reordering_window = self.observed_reordering_window.max(
                    received_at
                        .saturating_duration_since(since)
                        .min(MAX_REORDERING_WINDOW),
                );
            }
            return;
        }
        match entry.acknowledgment {
            Acknowledgment::Missing { since, .. } => {
                let observed = received_at
                    .saturating_duration_since(since)
                    .min(MAX_REORDERING_WINDOW);
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
                                .saturating_duration_since(since)
                                .min(MAX_REORDERING_WINDOW),
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
        received_at: Instant,
    ) {
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        if entry.acknowledgment.is_terminal()
            && !(entry.acknowledgment == Acknowledgment::Lost && entry.missing_since.is_some())
        {
            self.counters.duplicate_feedback = self.counters.duplicate_feedback.saturating_add(1);
            return;
        }
        let terminal_loss = entry.acknowledgment == Acknowledgment::Lost;
        if !terminal_loss {
            entry.acknowledgment = Acknowledgment::Received;
        }
        entry.missing_since = None;
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

    pub(super) fn set_missing(&mut self, sent_id: SentPacketId, now: Instant) {
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        match entry.acknowledgment {
            Acknowledgment::Pending => {
                entry.acknowledgment = Acknowledgment::Missing {
                    since: now,
                    higher_received_at: None,
                };
                entry.missing_since = Some(now);
                self.missing.push_back(sent_id);
                self.emit_observed(sent_id, false, false, None, None, None, Some(now));
            }
            Acknowledgment::Missing { .. }
            | Acknowledgment::Received
            | Acknowledgment::Lost
            | Acknowledgment::Retired => {}
        }
    }

    pub(super) fn confirm_losses(&mut self, now: Instant, limit: usize) -> usize {
        let mut work = 0;
        let scan_limit = limit
            .min(self.missing.len())
            .min(MAX_EXPIRATIONS_PER_POLL.saturating_sub(self.loss_work));
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
        if self.loss_work == MAX_EXPIRATIONS_PER_POLL {
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
            if let Some(expiry) = entry.committed_at.checked_add(SENT_HISTORY_MAX_AGE)
                && self
                    .reorder_deadline(&entry)
                    .is_some_and(|deadline| deadline <= expiry)
            {
                origin = LossOrigin::Reordering;
            }
        }
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        if entry.acknowledgment.is_terminal() {
            return;
        }
        entry.acknowledgment = Acknowledgment::Lost;
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
        observed_at: Option<Instant>,
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
        // Native TWCC PacketResults leave loss/recovery flags unset. Only CCFB
        // qualifies provisional receiver loss for SCReAM; ledger loss is separate.
        let reports_controller_loss = self.mode == Some(PacketFeedbackKind::Rfc8888);
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        let reported_lost_for_the_first_time = reports_controller_loss
            && !received
            && !entry.controller_loss_reported
            && (loss_origin == Some(LossOrigin::Reordering)
                || matches!(entry.acknowledgment, Acknowledgment::Missing { .. }));
        let recovered = received && entry.controller_loss_reported;
        if reported_lost_for_the_first_time {
            entry.controller_loss_reported = true;
        } else if recovered {
            entry.controller_loss_reported = false;
        }
        let sample = PacketFeedback {
            sent_id,
            ssrc: entry.ssrc,
            committed_at: entry.committed_at,
            transport_bytes: u32::try_from(entry.wire_len).unwrap_or(u32::MAX),
            service: entry.service,
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
            if self.inputs.synthetic.len() < MAX_EXPIRATIONS_PER_POLL {
                self.inputs.synthetic.push(sample);
            } else {
                self.inputs.exhausted = true;
            }
        } else if self.inputs.feedback.len() < MAX_FEEDBACK_STATUSES {
            self.inputs.feedback.push(sample);
        } else {
            self.inputs.exhausted = true;
        }
    }

    pub(super) fn twcc_sent_id(&self, sequence: u64) -> Option<SentPacketId> {
        self.twcc_index[ring_index(sequence)]
            .filter(|index| index.sequence == sequence)
            .map(|index| index.sent_id)
    }

    fn lookup_rtp(&self, ssrc_index: usize, sequence: u64) -> Option<SentPacketId> {
        let ids = &self.ssrcs[ssrc_index].sent_ids;
        let mut low = 0;
        let mut high = ids.len();
        while low < high {
            let middle = low + (high - low) / 2;
            let id = *ids.get(middle)?;
            let entry = self.entry(id)?;
            match entry.rtp_sequence.cmp(&sequence) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(id),
            }
        }
        None
    }
}
