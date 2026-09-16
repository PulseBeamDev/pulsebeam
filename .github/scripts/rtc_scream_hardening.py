from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    s = p.read_text()
    if old not in s:
        raise SystemExit(f"{label} not found")
    p.write_text(s.replace(old, new, 1))


def replace_all(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    s = p.read_text()
    if old not in s:
        raise SystemExit(f"{label} not found")
    p.write_text(s.replace(old, new))


# Sent-history lifetime and path-epoch hygiene.
replace_once(
    "crates/pulsebeam-rtc/src/sent_history.rs",
    "const INITIAL_REORDERING_WINDOW: Duration = Duration::from_millis(30);\n",
    "const INITIAL_REORDERING_WINDOW: Duration = Duration::from_millis(30);\nconst MAX_REORDERING_WINDOW: Duration = Duration::from_millis(500);\n",
    "reordering constants",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history.rs",
    """        if let Some(previous) = self.entries[slot] {\n            self.remove_indexes(previous);\n        }\n""",
    """        if let Some(previous) = self.entries[slot] {\n            self.remove_indexes(previous);\n            // Once a ring generation is overwritten, no earlier sent id can still be\n            // retained. Keep the expiration cursor on a live generation.\n            self.oldest_sent_id = self.oldest_sent_id.max(previous.id.0.saturating_add(1));\n        }\n""",
    "oldest cursor on overwrite",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history.rs",
    """        let rtp_sequence = if self.ssrcs[ssrc_index].sent_ids.is_empty() {\n            u64::from(rtp.sequence)\n        } else {\n            unwrap_forward(rtp.sequence, self.ssrcs[ssrc_index].newest_sequence)\n        };\n        self.ssrcs[ssrc_index].newest_sequence = rtp_sequence;\n""",
    """        let empty_history = self.ssrcs[ssrc_index].sent_ids.is_empty();\n        let rtp_sequence = if empty_history {\n            u64::from(rtp.sequence)\n        } else {\n            unwrap_forward(rtp.sequence, self.ssrcs[ssrc_index].newest_sequence)\n        };\n        if empty_history {\n            self.ssrcs[ssrc_index].first_sequence = rtp_sequence;\n        }\n        self.ssrcs[ssrc_index].newest_sequence = rtp_sequence;\n""",
    "rebase first RTP sequence",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history.rs",
    """        self.inputs.bytes_in_flight = self.bytes_in_flight;\n        self.inputs.path_change = Some(PathChange { epoch, available });\n""",
    """        self.inputs.bytes_in_flight = self.bytes_in_flight;\n        // Controller evidence is path scoped. Never let feedback emitted before a\n        // selected-path replacement reach the controller on the new epoch.\n        self.inputs.feedback.clear();\n        self.inputs.timing = None;\n        self.inputs.path_change = Some(PathChange { epoch, available });\n""",
    "clear old-path controller evidence",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history.rs",
    """        for ssrc in &mut self.ssrcs {\n            ssrc.last_report_timestamp = None;\n            ssrc.highest_acked_sequence = None;\n        }\n""",
    """        for ssrc in &mut self.ssrcs {\n            ssrc.last_report_timestamp = None;\n            ssrc.highest_acked_sequence = None;\n            ssrc.sent_ids.clear();\n        }\n""",
    "clear old-path per-ssrc progression",
)

# Feedback normalization: report-scoped progress, terminal ACK state, bounded loss delay,
# and conservative overflow behavior.
replace_once(
    "crates/pulsebeam-rtc/src/sent_history/feedback.rs",
    """        let sent = self\n            .entry(SentPacketId(self.oldest_sent_id))\n            .and_then(|entry| entry.committed_at.checked_add(SENT_HISTORY_MAX_AGE));\n""",
    """        let scan_end = self\n            .oldest_sent_id\n            .saturating_add(SENT_HISTORY_CAPACITY as u64)\n            .min(self.next_sent_id);\n        let sent = (self.oldest_sent_id..scan_end).find_map(|id| {\n            self.entry(SentPacketId(id))\n                .and_then(|entry| entry.committed_at.checked_add(SENT_HISTORY_MAX_AGE))\n        });\n""",
    "deadline skips ring holes",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history/feedback.rs",
    """        let start = self.highest_twcc_acked.map_or_else(\n            || self.oldest_twcc.unwrap_or(base),\n            |previous| previous.saturating_add(1),\n        );\n        if edge < start || edge.saturating_sub(start) > MAX_FEEDBACK_STATUSES as u64 {\n            return;\n        }\n""",
    """        let start = self.highest_twcc_acked.map_or(base, |previous| {\n            base.max(previous.saturating_add(1))\n        });\n        if edge < start || edge.saturating_sub(start) >= MAX_FEEDBACK_STATUSES as u64 {\n            return;\n        }\n        let edge_known = self\n            .twcc_sent_id(edge)\n            .and_then(|sent_id| self.entry(sent_id))\n            .is_some_and(|entry| entry.path_epoch == epoch);\n        if !edge_known {\n            self.add_unknown(1);\n            return;\n        }\n""",
    "TWCC report-scoped ACK start",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history/feedback.rs",
    """                let start = old_edge.map_or(self.ssrcs[ssrc_index].first_sequence, |previous| {\n                    previous.saturating_add(1)\n                });\n                if edge >= start && edge.saturating_sub(start) <= MAX_FEEDBACK_STATUSES as u64 {\n                    for sequence in start..=edge {\n                        let sent_id = self.lookup_rtp(ssrc_index, sequence);\n                        let offset = sequence\n                            .checked_sub(base)\n                            .and_then(|value| usize::try_from(value).ok());\n                        let (received, arrival, ecn, _) = offset\n                            .and_then(|index| normalized.get(index))\n                            .copied()\n                            .unwrap_or((false, None, None, None));\n                        self.advance_entry(sent_id, epoch, received, arrival, ecn, received_at);\n                    }\n                    self.ssrcs[ssrc_index].highest_acked_sequence = Some(edge);\n                }\n""",
    """                let start = old_edge.map_or(base, |previous| {\n                    base.max(previous.saturating_add(1))\n                });\n                let edge_known = self\n                    .lookup_rtp(ssrc_index, edge)\n                    .and_then(|sent_id| self.entry(sent_id))\n                    .is_some_and(|entry| entry.path_epoch == epoch);\n                if !edge_known {\n                    self.add_unknown(1);\n                } else if edge >= start\n                    && edge.saturating_sub(start) < MAX_FEEDBACK_STATUSES as u64\n                {\n                    for sequence in start..=edge {\n                        let sent_id = self.lookup_rtp(ssrc_index, sequence);\n                        let offset = sequence\n                            .checked_sub(base)\n                            .and_then(|value| usize::try_from(value).ok());\n                        let (received, arrival, ecn, _) = offset\n                            .and_then(|index| normalized.get(index))\n                            .copied()\n                            .unwrap_or((false, None, None, None));\n                        self.advance_entry(sent_id, epoch, received, arrival, ecn, received_at);\n                    }\n                    self.ssrcs[ssrc_index].highest_acked_sequence = Some(edge);\n                }\n""",
    "RFC8888 report-scoped ACK start",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history/feedback.rs",
    """                self.reordering_window = self\n                    .reordering_window\n                    .max(received_at.saturating_duration_since(since));\n""",
    """                let observed = received_at\n                    .saturating_duration_since(since)\n                    .min(MAX_REORDERING_WINDOW);\n                self.reordering_window = self.reordering_window.max(observed);\n""",
    "bounded reordering growth",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history/feedback.rs",
    """        if matches!(entry.acknowledgment, Acknowledgment::Received) {\n            self.counters.duplicate_feedback = self.counters.duplicate_feedback.saturating_add(1);\n            return;\n        }\n""",
    """        if entry.acknowledgment.is_terminal() {\n            self.counters.duplicate_feedback = self.counters.duplicate_feedback.saturating_add(1);\n            return;\n        }\n""",
    "terminal ACK state is monotonic",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history/feedback.rs",
    """    fn confirm_losses(&mut self, now: Instant, limit: usize) -> usize {\n        let mut work = 0;\n        while work < limit {\n""",
    """    fn confirm_losses(&mut self, now: Instant, limit: usize) -> usize {\n        let mut work = 0;\n        let mut confirmed_loss = false;\n        while work < limit {\n""",
    "loss decay state",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history/feedback.rs",
    """                    self.missing.pop_front();\n                    self.mark_lost(sent_id, false);\n                    work += 1;\n""",
    """                    self.missing.pop_front();\n                    self.mark_lost(sent_id, false);\n                    confirmed_loss = true;\n                    work += 1;\n""",
    "mark confirmed loss for decay",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history/feedback.rs",
    """        }\n        work\n    }\n\n    fn mark_lost(&mut self, sent_id: SentPacketId, retire_from_flight: bool) {\n""",
    """        }\n        if confirmed_loss && self.reordering_window > INITIAL_REORDERING_WINDOW {\n            let excess = self\n                .reordering_window\n                .saturating_sub(INITIAL_REORDERING_WINDOW);\n            self.reordering_window =\n                INITIAL_REORDERING_WINDOW.saturating_add(excess / 2);\n        }\n        work\n    }\n\n    fn mark_lost(&mut self, sent_id: SentPacketId, retire_from_flight: bool) {\n""",
    "decay reordering window after loss batch",
)
replace_once(
    "crates/pulsebeam-rtc/src/sent_history/feedback.rs",
    """        if self.inputs.feedback.len() >= MAX_FEEDBACK_STATUSES {\n            return;\n        }\n        let Some(entry) = self.entry(sent_id).copied() else {\n            return;\n        };\n        self.inputs.feedback.push(PacketFeedback {\n            sent_id,\n            committed_at: entry.committed_at,\n            transport_bytes: u32::try_from(entry.wire_len).unwrap_or(u32::MAX),\n            service: entry.service,\n            received,\n            newly_acked,\n            lost,\n            receiver_arrival,\n            ecn,\n        });\n""",
    """        let Some(entry) = self.entry(sent_id).copied() else {\n            return;\n        };\n        let sample = PacketFeedback {\n            sent_id,\n            committed_at: entry.committed_at,\n            transport_bytes: u32::try_from(entry.wire_len).unwrap_or(u32::MAX),\n            service: entry.service,\n            received,\n            newly_acked,\n            lost,\n            receiver_arrival,\n            ecn,\n        };\n        if self.inputs.feedback.len() >= MAX_FEEDBACK_STATUSES {\n            // A confirmed loss must never disappear merely because one RTCP batch filled\n            // the bounded vector. Replace lower-severity evidence so the controller takes\n            // a conservative congestion path while preserving the hard work bound.\n            if lost\n                && let Some(existing) = self.inputs.feedback.iter_mut().find(|item| !item.lost)\n            {\n                *existing = sample;\n            }\n            return;\n        }\n        self.inputs.feedback.push(sample);\n""",
    "loss-priority feedback overflow",
)

# SCReAM RTT smoothing is one sample per feedback batch, using the latest sent received
# unit rather than applying the same RTCP arrival instant to every status.
replace_once(
    "crates/pulsebeam-rtc/src/congestion/screamv2.rs",
    """        let mut delivered_bytes = 0_u64;\n        let mut interval_start = now;\n        let mut loss_events = 0_u64;\n        for sample in input.feedback {\n""",
    """        let mut delivered_bytes = 0_u64;\n        let mut interval_start = now;\n        let mut loss_events = 0_u64;\n        let rtt_sample = input\n            .feedback\n            .iter()\n            .filter(|sample| sample.received)\n            .max_by_key(|sample| sample.sent_at);\n        for sample in input.feedback {\n""",
    "single RTT sample selection",
)
replace_once(
    "crates/pulsebeam-rtc/src/congestion/screamv2.rs",
    """                let raw_rtt = sample\n                    .received_at\n                    .saturating_sub(sample.sent_at)\n                    .saturating_sub(input.feedback_hold);\n                self.s_rtt = ewma_duration(self.s_rtt, raw_rtt, 1, 8);\n                self.observe_delay(sample);\n""",
    """                self.observe_delay(sample);\n""",
    "remove per-status RTT EWMA",
)
replace_once(
    "crates/pulsebeam-rtc/src/congestion/screamv2.rs",
    """        let interval = now\n            .saturating_sub(interval_start)\n            .max(Duration::from_millis(1));\n""",
    """        if let Some(sample) = rtt_sample {\n            let raw_rtt = sample\n                .received_at\n                .saturating_sub(sample.sent_at)\n                .saturating_sub(input.feedback_hold);\n            self.s_rtt = ewma_duration(self.s_rtt, raw_rtt, 1, 8);\n        }\n        let interval = now\n            .saturating_sub(interval_start)\n            .max(Duration::from_millis(1));\n""",
    "apply one RTT EWMA",
)

# Remove a vacuous scenario metric: path isolation is already evidenced by the reset count
# and the sent-history epoch tests, so a hard-coded false flag is misleading.
replace_all(
    "crates/pulsebeam-rtc/src/congestion/scenario.rs",
    "    pub(crate) old_path_sample_used: bool,\n",
    "",
    "old-path metric field",
)
replace_all(
    "crates/pulsebeam-rtc/src/congestion/scenario.rs",
    "            old_path_sample_used: false,\n",
    "",
    "old-path metric default",
)
replace_all(
    "crates/pulsebeam-rtc/src/congestion/scenario.rs",
    "    metrics.old_path_sample_used = false;\n",
    "",
    "old-path metric assignment",
)
replace_once(
    "crates/pulsebeam-rtc/tests/scream_scenarios.rs",
    """                assert_eq!(metrics.baseline_resets, 1);\n                assert!(!metrics.old_path_sample_used);\n""",
    """                assert_eq!(metrics.baseline_resets, 1);\n""",
    "remove vacuous old-path assertion",
)

# Add focused acceptance evidence for the outer safety contract and RTT batch sampling.
congestion = Path("crates/pulsebeam-rtc/src/congestion.rs")
s = congestion.read_text()
marker = """    #[test]\n    fn latency_governor_remains_outer_policy_only() {\n"""
if marker not in s:
    raise SystemExit("congestion test insertion marker not found")
extra = r'''    #[test]
    fn application_limited_feedback_does_not_inflate_reference_window() {
        let mut cc = ScreamController::new(4_000_000, None);
        cc.note_send(Duration::ZERO);
        let base = ControllerInput {
            path_epoch: Some(1),
            path_available: true,
            feedback: &[],
            feedback_hold: Duration::ZERO,
            fresh_network_feedback: false,
            bytes_in_flight: 1_000,
            paced_queue_bytes: 0,
            offered_media_rate: 64_000,
            admitted_media_rate: 64_000,
            desired_media_rate: 4_000_000,
            window_or_pacer_blocked: false,
            ecn: EcnValidation::default(),
        };
        cc.update(Duration::ZERO, base);
        let entered = cc.update(Duration::from_millis(250), base);
        assert!(entered.application_limited);
        let before = entered.reference_window;
        let feedback = [FeedbackSample {
            sent_at: Duration::from_millis(240),
            received_at: Duration::from_millis(300),
            transport_bytes: 1_000,
            received: true,
            newly_acked: true,
            lost: false,
            receiver_arrival_micros: None,
            ecn: None,
        }];
        let after = cc.update(
            Duration::from_millis(300),
            ControllerInput {
                feedback: &feedback,
                fresh_network_feedback: true,
                ..base
            },
        );
        assert!(after.application_limited);
        assert_eq!(after.reference_window, before);
    }

    #[test]
    fn confidence_has_five_second_half_life_while_application_limited() {
        let mut cc = ScreamController::new(2_000_000, None);
        cc.note_send(Duration::ZERO);
        let input = ControllerInput {
            path_epoch: Some(1),
            path_available: true,
            feedback: &[],
            feedback_hold: Duration::ZERO,
            fresh_network_feedback: false,
            bytes_in_flight: 0,
            paced_queue_bytes: 0,
            offered_media_rate: 64_000,
            admitted_media_rate: 64_000,
            desired_media_rate: 2_000_000,
            window_or_pacer_blocked: false,
            ecn: EcnValidation::default(),
        };
        cc.update(Duration::ZERO, input);
        let started = cc.update(Duration::from_millis(250), input);
        assert!(started.application_limited);
        let half = cc.update(Duration::from_millis(5_250), input);
        assert_eq!(half.queue_delay_confidence, u16::MAX / 2);
    }

'''
s = s.replace(marker, extra + marker, 1)
congestion.write_text(s)

scream = Path("crates/pulsebeam-rtc/src/congestion/screamv2.rs")
s = scream.read_text()
marker = """    #[test]\n    fn path_reset_drops_previous_capacity_state() {\n"""
if marker not in s:
    raise SystemExit("SCReAM test insertion marker not found")
extra = r'''    #[test]
    fn rtt_filter_consumes_one_sample_per_feedback_batch() {
        let mut cc = ScreamV2::new(4_000_000, None);
        let feedback = [
            FeedbackSample {
                sent_at: Duration::ZERO,
                received_at: Duration::from_millis(200),
                transport_bytes: 1_000,
                received: true,
                newly_acked: true,
                lost: false,
                receiver_arrival_micros: None,
                ecn: None,
            },
            FeedbackSample {
                sent_at: Duration::from_millis(190),
                received_at: Duration::from_millis(200),
                transport_bytes: 1_000,
                received: true,
                newly_acked: true,
                lost: false,
                receiver_arrival_micros: None,
                ecn: None,
            },
        ];
        cc.consume_feedback(Duration::from_millis(200), input(&feedback, 4_000_000));
        assert_eq!(cc.s_rtt, Duration::from_micros(88_750));
    }

'''
s = s.replace(marker, extra + marker, 1)
scream.write_text(s)

sent_tests = Path("crates/pulsebeam-rtc/src/sent_history/tests.rs")
s = sent_tests.read_text()
s += r'''

#[test]
fn path_change_discards_pending_feedback_and_rebases_ssrc_history() {
    let now = Instant::now();
    let old_epoch = PathEpoch::from_value(20);
    let new_epoch = PathEpoch::from_value(21);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(old_epoch, true);
    history.commit(rfc_context(now, old_epoch, 10)).unwrap();
    history.emit(SentPacketId(0), false, false, true, None, None);
    assert!(!history.inputs.feedback.is_empty());
    history.path_changed(new_epoch, true);
    assert!(history.inputs.feedback.is_empty());
    assert!(history.inputs.timing.is_none());
    assert!(history.ssrcs[0].sent_ids.is_empty());
    history.commit(rfc_context(now, new_epoch, 500)).unwrap();
    assert_eq!(history.ssrcs[0].first_sequence, 500);
}

#[test]
fn reordering_window_is_bounded_and_decays_after_confirmed_loss() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(22);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    history.commit(rfc_context(now, epoch, 1)).unwrap();
    history.set_missing(SentPacketId(0), now);
    history.recover(
        Some(SentPacketId(0)),
        epoch,
        None,
        None,
        now + Duration::from_secs(2),
    );
    assert_eq!(history.reordering_window, MAX_REORDERING_WINDOW);

    history.commit(rfc_context(now, epoch, 2)).unwrap();
    history.set_missing(SentPacketId(1), now);
    history.confirm_losses(now + MAX_REORDERING_WINDOW, 1);
    assert!(history.reordering_window < MAX_REORDERING_WINDOW);
    assert!(history.reordering_window >= INITIAL_REORDERING_WINDOW);
}
'''
sent_tests.write_text(s)

# Reconcile all normative/high-level docs with the isolated core boundary and make
# aggregate-demand routing explicit.
replace_once(
    "crates/pulsebeam-rtc/README.md",
    """The SCReAM core remains independently testable and self-contained. A private\nPulseBeam latency governor may only tighten its native queue-delay target; it\ncannot increase SCReAM's congestion window, pacing permission, or estimated\ncapacity.\n""",
    """The SCReAM core remains independently testable and self-contained. PulseBeam\nlatency policy governs admission, allocation, pacing horizons, retransmission, and\nprobing outside that core; it does not override SCReAM's native queue-delay target,\ncongestion window, pacing equations, or estimated capacity.\n""",
    "README SCReAM ownership",
)
replace_once(
    "crates/pulsebeam-rtc/docs/design.md",
    """- one private PulseBeam latency governor that may only tighten the core's native\n  queue-delay target through an upper ceiling;\n""",
    """- one private PulseBeam latency governor for admission, allocation, pacer horizons,\n  retransmission, and probing outside the SCReAM core;\n""",
    "design SCReAM ownership",
)
replace_once(
    "crates/pulsebeam-rtc/docs/congestion-control.md",
    """                per-sender operating points\n                              |\n                              v\npacket feedback ------> self-contained SCReAM v2 core ----------> safe RTP envelope\n""",
    """                per-sender operating points\n                              |\n                              v\n                 external demand aggregation\n                              |\n                              v\npacket feedback ------> self-contained SCReAM v2 core ----------> safe RTP envelope\n""",
    "diagram aggregate demand",
)
