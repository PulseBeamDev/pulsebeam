use super::*;

impl SentHistory {
    pub(crate) fn process_feedback(&mut self, batch: FeedbackBatch) {
        self.inputs.timing = None;
        if self.active_epoch != Some(batch.path_epoch) {
            let count = feedback_status_count(&batch.report);
            self.counters.wrong_path_feedback = self
                .counters
                .wrong_path_feedback
                .saturating_add(count as u64);
            return;
        }
        let received_at = batch.received_at.monotonic;
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
        self.confirm_losses(received_at, MAX_EXPIRATIONS_PER_POLL);
        let newest_send_age = self
            .inputs
            .feedback
            .iter()
            .filter_map(|feedback| self.entry(feedback.sent_id))
            .map(|entry| received_at.saturating_duration_since(entry.committed_at))
            .min();
        self.inputs.timing = Some(FeedbackTiming {
            received_at,
            feedback_hold,
            newest_send_age,
        });
    }

    pub(crate) fn expire(&mut self, now: Instant, limit: usize) -> usize {
        let mut work = self.confirm_losses(now, limit);
        while work < limit && self.oldest_sent_id < self.next_sent_id {
            let id = SentPacketId(self.oldest_sent_id);
            let Some(entry) = self.entry(id).copied() else {
                self.oldest_sent_id = self.oldest_sent_id.saturating_add(1);
                work += 1;
                continue;
            };
            let Some(deadline) = entry.committed_at.checked_add(SENT_HISTORY_MAX_AGE) else {
                break;
            };
            if now < deadline {
                break;
            }
            if !entry.acknowledgment.is_terminal() {
                self.mark_lost(id, true);
                self.counters.expired = self.counters.expired.saturating_add(1);
            }
            self.oldest_sent_id = self.oldest_sent_id.saturating_add(1);
            work += 1;
        }
        work
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let sent = self
            .entry(SentPacketId(self.oldest_sent_id))
            .and_then(|entry| entry.committed_at.checked_add(SENT_HISTORY_MAX_AGE));
        let missing = self.missing.iter().find_map(|id| {
            let entry = self.entry(*id)?;
            match entry.acknowledgment {
                Acknowledgment::Missing { since } => since.checked_add(self.reordering_window),
                _ => None,
            }
        });
        match (sent, missing) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, None) | (None, left) => left,
        }
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
        let Some(newest) = self.newest_twcc else {
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
        let reference = unwrap_near_signed(i64::from(reference_time), self.twcc_reference, 1 << 24);
        self.twcc_reference = Some(reference);
        let count = unwrap_near_signed(i64::from(feedback_count), self.twcc_feedback_count, 1 << 8);
        if self
            .twcc_feedback_count
            .is_some_and(|previous| count <= previous)
        {
            self.counters.reordered_feedback = self.counters.reordered_feedback.saturating_add(1);
        } else {
            self.twcc_feedback_count = Some(count);
        }

        let mut arrivals = Vec::with_capacity(statuses.len());
        let mut arrival_micros = reference.saturating_mul(64_000);
        let mut highest_received = None;
        for (offset, status) in statuses.iter().enumerate() {
            let sequence = base.saturating_add(offset as u64);
            let arrival = match status {
                TwccStatus::NotReceived => None,
                TwccStatus::Received { delta_250us } => {
                    arrival_micros =
                        arrival_micros.saturating_add(i64::from(*delta_250us).saturating_mul(250));
                    highest_received = Some(sequence);
                    Some(ReceiverTime::Micros(arrival_micros))
                }
            };
            arrivals.push(arrival);
        }
        let old_edge = self.highest_twcc_acked;
        if let Some(edge) = highest_received {
            self.advance_twcc_edge(base, statuses, &arrivals, edge, epoch, received_at);
        }
        for (offset, status) in statuses.iter().enumerate() {
            let sequence = base.saturating_add(offset as u64);
            if old_edge.is_some_and(|edge| sequence <= edge)
                && matches!(status, TwccStatus::Received { .. })
            {
                let sent_id = self.twcc_sent_id(sequence);
                self.recover(sent_id, epoch, arrivals[offset], None, received_at);
            }
        }
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
            |previous| previous.saturating_add(1),
        );
        if edge < start || edge.saturating_sub(start) > MAX_FEEDBACK_STATUSES as u64 {
            return;
        }
        for sequence in start..=edge {
            let sent_id = self.twcc_sent_id(sequence);
            let offset = sequence
                .checked_sub(base)
                .and_then(|value| usize::try_from(value).ok());
            let received = offset
                .and_then(|index| statuses.get(index))
                .is_some_and(|status| matches!(status, TwccStatus::Received { .. }));
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
            let timestamp = unwrap_near_signed(
                i64::from(report_timestamp),
                self.ssrcs[ssrc_index].last_report_timestamp,
                1_i64 << 32,
            );
            self.ssrcs[ssrc_index].last_report_timestamp = Some(timestamp);
            let report_micros = timestamp.saturating_mul(1_000_000) / 65_536;
            let mut normalized = Vec::with_capacity(report.statuses.len());
            let mut highest_received = None;
            for (offset, status) in report.statuses.iter().enumerate() {
                let sequence = base.saturating_add(offset as u64);
                let value = match status {
                    Rfc8888Status::NotReceived => (false, None, None, None),
                    Rfc8888Status::Received {
                        ecn,
                        arrival_offset,
                    } => {
                        highest_received = Some(sequence);
                        match arrival_offset {
                            ArrivalOffset::Ticks(ticks) => {
                                let hold_micros =
                                    u64::from(*ticks).saturating_mul(1_000_000) / 1_024;
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
                        }
                    }
                };
                if let Some(hold) = value.3 {
                    minimum_hold = Some(minimum_hold.map_or(hold, |known| known.min(hold)));
                }
                normalized.push(value);
            }
            let old_edge = self.ssrcs[ssrc_index].highest_acked_sequence;
            if let Some(edge) = highest_received {
                let start = old_edge.map_or(self.ssrcs[ssrc_index].first_sequence, |previous| {
                    previous.saturating_add(1)
                });
                if edge >= start && edge.saturating_sub(start) <= MAX_FEEDBACK_STATUSES as u64 {
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
            for (offset, (received, arrival, ecn, _)) in normalized.iter().copied().enumerate() {
                let sequence = base.saturating_add(offset as u64);
                if received && old_edge.is_some_and(|edge| sequence <= edge) {
                    let sent_id = self.lookup_rtp(ssrc_index, sequence);
                    self.recover(sent_id, epoch, arrival, ecn, received_at);
                }
            }
        }
        minimum_hold
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
        self.retire_from_flight(sent_id);
        if received {
            self.set_received(sent_id, true, receiver_arrival, ecn, received_at);
        } else {
            self.set_missing(sent_id, received_at);
            self.emit(sent_id, false, true, false, None, None);
        }
    }

    fn recover(
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
        match entry.acknowledgment {
            Acknowledgment::Missing { since } => {
                self.reordering_window = self
                    .reordering_window
                    .max(received_at.saturating_duration_since(since));
                self.set_received(sent_id, false, receiver_arrival, ecn, received_at);
            }
            Acknowledgment::Received | Acknowledgment::Lost | Acknowledgment::Retired => {
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
        _received_at: Instant,
    ) {
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        if matches!(entry.acknowledgment, Acknowledgment::Received) {
            self.counters.duplicate_feedback = self.counters.duplicate_feedback.saturating_add(1);
            return;
        }
        entry.acknowledgment = Acknowledgment::Received;
        self.counters.received = self.counters.received.saturating_add(1);
        self.emit(sent_id, true, newly_acked, false, receiver_arrival, ecn);
    }

    fn set_missing(&mut self, sent_id: SentPacketId, now: Instant) {
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        match entry.acknowledgment {
            Acknowledgment::Pending => {
                entry.acknowledgment = Acknowledgment::Missing { since: now };
                self.missing.push_back(sent_id);
            }
            Acknowledgment::Missing { .. }
            | Acknowledgment::Received
            | Acknowledgment::Lost
            | Acknowledgment::Retired => {}
        }
    }

    fn confirm_losses(&mut self, now: Instant, limit: usize) -> usize {
        let mut work = 0;
        while work < limit {
            let Some(sent_id) = self.missing.front().copied() else {
                break;
            };
            let Some(entry) = self.entry(sent_id).copied() else {
                self.missing.pop_front();
                work += 1;
                continue;
            };
            match entry.acknowledgment {
                Acknowledgment::Missing { since } => {
                    let Some(deadline) = since.checked_add(self.reordering_window) else {
                        break;
                    };
                    if now < deadline {
                        break;
                    }
                    self.missing.pop_front();
                    self.mark_lost(sent_id, false);
                    work += 1;
                }
                _ => {
                    self.missing.pop_front();
                    work += 1;
                }
            }
        }
        work
    }

    fn mark_lost(&mut self, sent_id: SentPacketId, retire_from_flight: bool) {
        if retire_from_flight {
            self.retire_from_flight(sent_id);
        }
        let Some(entry) = self.entry_mut(sent_id) else {
            return;
        };
        if entry.acknowledgment.is_terminal() {
            return;
        }
        entry.acknowledgment = Acknowledgment::Lost;
        self.counters.not_received = self.counters.not_received.saturating_add(1);
        self.emit(sent_id, false, false, true, None, None);
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

    fn emit(
        &mut self,
        sent_id: SentPacketId,
        received: bool,
        newly_acked: bool,
        lost: bool,
        receiver_arrival: Option<ReceiverTime>,
        ecn: Option<EcnMark>,
    ) {
        if self.inputs.feedback.len() >= MAX_FEEDBACK_STATUSES {
            return;
        }
        let Some(entry) = self.entry(sent_id).copied() else {
            return;
        };
        self.inputs.feedback.push(PacketFeedback {
            sent_id,
            committed_at: entry.committed_at,
            transport_bytes: u32::try_from(entry.wire_len).unwrap_or(u32::MAX),
            service: entry.service,
            received,
            newly_acked,
            lost,
            receiver_arrival,
            ecn,
        });
    }

    fn twcc_sent_id(&self, sequence: u64) -> Option<SentPacketId> {
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
