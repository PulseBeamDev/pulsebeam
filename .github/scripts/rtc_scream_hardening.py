from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    s = p.read_text()
    if old not in s:
        raise SystemExit(f"{label} not found")
    p.write_text(s.replace(old, new, 1))


# A selected-path replacement must not inherit the previous path's capacity state.
replace_once(
    "crates/pulsebeam-rtc/src/congestion/screamv2.rs",
    '''    /// Transport/path replacement invalidates one-way-delay and ECN evidence, not capacity state.\n    pub(crate) fn reset_path_evidence(&mut self) {\n        self.base_delay_minima.clear();\n        self.competing_samples.clear();\n        self.qdelay = Duration::ZERO;\n        self.qdelay_avg = Duration::ZERO;\n        self.qdelay_max_avg = self.qdelay_target;\n        self.qdelay_min_avg = Duration::ZERO;\n        self.qdelay_dev_avg = Duration::ZERO;\n        self.ref_wnd_delay_scale = ONE;\n        self.l4s_alpha = 0;\n        self.ecn_mode = EcnMode::Disabled;\n        self.data_units_delivered_this_rtt = 0;\n        self.data_units_marked_this_rtt = 0;\n        self.reason = ControllerReason::PathChanged;\n    }\n''',
    '''    /// A selected-path replacement invalidates path capacity as well as delay/ECN evidence.\n    pub(crate) fn reset_path(&mut self, target_bitrate_max: u64) {\n        let path_payload_max = u32::try_from(self.mss).ok();\n        *self = Self::new(target_bitrate_max, path_payload_max);\n        self.reason = ControllerReason::PathChanged;\n    }\n''',
    "path reset",
)

# A recovered reordered CE mark is newly learned congestion evidence even though its
# sequence-space ACK credit was accounted when the ACK edge first passed it.
replace_once(
    "crates/pulsebeam-rtc/src/congestion/screamv2.rs",
    '''            if sample.newly_acked {\n                self.bytes_newly_acked = self\n                    .bytes_newly_acked\n                    .saturating_add(u64::from(sample.transport_bytes));\n                if sample.received && sample.ecn == Some(EcnMark::Ce) {\n                    self.bytes_newly_acked_ce = self\n                        .bytes_newly_acked_ce\n                        .saturating_add(u64::from(sample.transport_bytes));\n                }\n            }\n''',
    '''            if sample.newly_acked {\n                self.bytes_newly_acked = self\n                    .bytes_newly_acked\n                    .saturating_add(u64::from(sample.transport_bytes));\n            }\n            if sample.received && sample.ecn == Some(EcnMark::Ce) {\n                self.bytes_newly_acked_ce = self\n                    .bytes_newly_acked_ce\n                    .saturating_add(u64::from(sample.transport_bytes));\n            }\n''',
    "recovered CE accounting",
)

core = Path("crates/pulsebeam-rtc/src/congestion/screamv2.rs")
s = core.read_text()
marker = '''    #[test]\n    fn timer_only_poll_does_not_replay_stale_delay_evidence() {'''
extra = '''    #[test]\n    fn path_reset_drops_previous_capacity_state() {\n        let mut cc = ScreamV2::new(8_000_000, None);\n        cc.ref_wnd = 200_000;\n        cc.target_bitrate = 8_000_000;\n        cc.s_rtt = Duration::from_millis(350);\n        cc.qdelay_target = Duration::from_millis(400);\n        cc.reset_path(2_000_000);\n        assert_eq!(cc.s_rtt, PROFILE.initial_rtt);\n        assert_eq!(cc.qdelay_target, PROFILE.queue_target_low);\n        assert!(cc.ref_wnd < 200_000);\n        assert!(cc.target_bitrate <= PROFILE.target_initial_bps);\n        assert_eq!(cc.reason, ControllerReason::PathChanged);\n    }\n\n    #[test]\n    fn recovered_ce_is_counted_without_double_ack_credit() {\n        let mut cc = ScreamV2::new(4_000_000, None);\n        let first = [sample(25, false, true, false, false)];\n        cc.consume_feedback(Duration::from_millis(25), input(&first, 4_000_000));\n        assert_eq!(cc.debug_accumulated_acks(), (1_000, 0));\n        let recovered = [sample(50, true, false, false, true)];\n        cc.consume_feedback(Duration::from_millis(50), input(&recovered, 4_000_000));\n        assert_eq!(cc.debug_accumulated_acks(), (1_000, 1_000));\n    }\n\n'''
if marker not in s:
    raise SystemExit("core test insertion marker not found")
core.write_text(s.replace(marker, extra + marker, 1))

# Distinguish an actual receiver report covering a committed packet from synthetic
# timer-confirmed loss events. Only the former refreshes feedback freshness/confidence.
replace_once(
    "crates/pulsebeam-rtc/src/congestion.rs",
    '''    pub(crate) feedback: &'a [FeedbackSample],\n    pub(crate) feedback_hold: Duration,\n''',
    '''    pub(crate) feedback: &'a [FeedbackSample],\n    pub(crate) feedback_hold: Duration,\n    pub(crate) fresh_network_feedback: bool,\n''',
    "fresh feedback field",
)
replace_once(
    "crates/pulsebeam-rtc/src/congestion.rs",
    '''        self.update_path(input.path_epoch, input.path_available);\n''',
    '''        self.update_path(\n            input.path_epoch,\n            input.path_available,\n            input.desired_media_rate.min(TARGET_BITRATE_MAX),\n        );\n''',
    "update path call",
)
replace_once(
    "crates/pulsebeam-rtc/src/congestion.rs",
    '''        if !input.feedback.is_empty() {\n            self.last_feedback = Some(now);\n            self.feedback_stale = false;\n        }\n        self.update_staleness(now);\n        self.update_confidence(now, !input.feedback.is_empty());\n''',
    '''        if input.fresh_network_feedback {\n            self.last_feedback = Some(now);\n            self.feedback_stale = false;\n        }\n        self.update_staleness(now);\n        self.update_confidence(now, input.fresh_network_feedback);\n''',
    "fresh feedback use",
)
replace_once(
    "crates/pulsebeam-rtc/src/congestion.rs",
    '''    fn update_path(&mut self, epoch: Option<u64>, available: bool) {\n        if self.path_epoch == epoch && self.path_available == available {\n            return;\n        }\n        self.path_epoch = epoch;\n        self.path_available = available;\n        self.last_feedback = None;\n        self.feedback_stale = false;\n        self.core.reset_path_evidence();\n        self.last_reason = ControllerReason::PathChanged;\n    }\n''',
    '''    fn update_path(&mut self, epoch: Option<u64>, available: bool, target_bitrate_max: u64) {\n        if self.path_epoch == epoch && self.path_available == available {\n            return;\n        }\n        self.path_epoch = epoch;\n        self.path_available = available;\n        self.last_feedback = None;\n        self.feedback_stale = false;\n        self.core.reset_path(target_bitrate_max);\n        self.last_reason = ControllerReason::PathChanged;\n    }\n''',
    "outer path reset",
)
replace_once(
    "crates/pulsebeam-rtc/src/congestion.rs",
    '''            feedback_hold: self.feedback_hold,\n            bytes_in_flight,\n''',
    '''            feedback_hold: self.feedback_hold,\n            fresh_network_feedback: false,\n            bytes_in_flight,\n''',
    "snapshot fresh feedback",
)
# First test base constructor in this file.
replace_once(
    "crates/pulsebeam-rtc/src/congestion.rs",
    '''            feedback_hold: Duration::ZERO,\n            bytes_in_flight: 0,\n''',
    '''            feedback_hold: Duration::ZERO,\n            fresh_network_feedback: false,\n            bytes_in_flight: 0,\n''',
    "congestion test fresh feedback",
)

congestion = Path("crates/pulsebeam-rtc/src/congestion.rs")
s = congestion.read_text()
insert = '''\n    #[test]\n    fn synthetic_loss_does_not_refresh_feedback_freshness() {\n        let mut cc = ScreamController::new(2_000_000, None);\n        cc.note_send(Duration::ZERO);\n        let base = ControllerInput {\n            path_epoch: Some(1),\n            path_available: true,\n            feedback: &[],\n            feedback_hold: Duration::ZERO,\n            fresh_network_feedback: false,\n            bytes_in_flight: 0,\n            paced_queue_bytes: 0,\n            offered_media_rate: 2_000_000,\n            admitted_media_rate: 2_000_000,\n            desired_media_rate: 2_000_000,\n            window_or_pacer_blocked: false,\n            queue_delay_ceiling: Duration::from_millis(15),\n            ecn: EcnValidation::default(),\n        };\n        cc.update(Duration::from_millis(600), base);\n        assert!(cc.feedback_stale);\n        let loss = [FeedbackSample {\n            sent_at: Duration::ZERO,\n            received_at: Duration::from_millis(600),\n            transport_bytes: 1_000,\n            received: false,\n            newly_acked: false,\n            lost: true,\n            receiver_arrival_micros: None,\n            ecn: None,\n        }];\n        let output = cc.update(\n            Duration::from_millis(650),\n            ControllerInput {\n                feedback: &loss,\n                fresh_network_feedback: false,\n                ..base\n            },\n        );\n        assert!(output.feedback_stale);\n        assert_eq!(cc.last_feedback, None);\n    }\n'''
marker = '''    #[test]\n    fn latency_governor_remains_outer_policy_only() {'''
if marker not in s:
    raise SystemExit("congestion test marker not found")
congestion.write_text(s.replace(marker, insert + "\n" + marker, 1))

# Wire receiver-feedback freshness through the connection/egress adapter.
replace_once(
    "crates/pulsebeam-rtc/src/egress.rs",
    '''        feedback: &[FeedbackSample],\n        feedback_hold: Duration,\n        bytes_in_flight: u64,\n''',
    '''        feedback: &[FeedbackSample],\n        feedback_hold: Duration,\n        fresh_network_feedback: bool,\n        bytes_in_flight: u64,\n''',
    "egress signature",
)
replace_once(
    "crates/pulsebeam-rtc/src/egress.rs",
    '''                feedback_hold,\n                bytes_in_flight,\n''',
    '''                feedback_hold,\n                fresh_network_feedback,\n                bytes_in_flight,\n''',
    "egress controller input",
)
replace_once(
    "crates/pulsebeam-rtc/src/connection.rs",
    '''        let (path_change, feedback, feedback_hold, bytes_in_flight) = {\n''',
    '''        let (path_change, feedback, feedback_hold, fresh_network_feedback, bytes_in_flight) = {\n''',
    "connection input tuple",
)
replace_once(
    "crates/pulsebeam-rtc/src/connection.rs",
    '''                inputs\n                    .timing\n                    .and_then(|timing| timing.feedback_hold)\n                    .unwrap_or_default(),\n                inputs.bytes_in_flight,\n''',
    '''                inputs\n                    .timing\n                    .and_then(|timing| timing.feedback_hold)\n                    .unwrap_or_default(),\n                inputs\n                    .timing\n                    .is_some_and(|timing| timing.newest_send_age.is_some()),\n                inputs.bytes_in_flight,\n''',
    "connection fresh feedback derivation",
)
replace_once(
    "crates/pulsebeam-rtc/src/connection.rs",
    '''            &feedback,\n            feedback_hold,\n            bytes_in_flight,\n''',
    '''            &feedback,\n            feedback_hold,\n            fresh_network_feedback,\n            bytes_in_flight,\n''',
    "connection egress call",
)

# Scenario network batches are actual receiver feedback; empty samples are timer-only.
replace_once(
    "crates/pulsebeam-rtc/src/congestion/scenario.rs",
    '''                feedback_hold: Duration::ZERO,\n                bytes_in_flight,\n''',
    '''                feedback_hold: Duration::ZERO,\n                fresh_network_feedback: !samples.is_empty(),\n                bytes_in_flight,\n''',
    "scenario fresh feedback",
)

# Reject future feedback before it can mutate TWCC/RFC8888 report/ACK state.
feedback = Path("crates/pulsebeam-rtc/src/sent_history/feedback.rs")
s = feedback.read_text()
old = '''        if newest.saturating_sub(base) > MAX_ACK_REORDERING {\n            self.counters.stale_feedback = self\n                .counters\n                .stale_feedback\n                .saturating_add(statuses.len() as u64);\n            return;\n        }\n'''
new = '''        if newest.saturating_sub(base) > MAX_ACK_REORDERING {\n            self.counters.stale_feedback = self\n                .counters\n                .stale_feedback\n                .saturating_add(statuses.len() as u64);\n            return;\n        }\n        let last_reported = base.saturating_add(\n            u64::try_from(statuses.len().saturating_sub(1)).unwrap_or(u64::MAX),\n        );\n        if base > newest || last_reported > newest {\n            self.add_unknown(statuses.len());\n            return;\n        }\n'''
if old not in s:
    raise SystemExit("TWCC stale block not found")
s = s.replace(old, new, 1)
old = '''            if newest.saturating_sub(base) > MAX_ACK_REORDERING {\n                self.counters.stale_feedback = self\n                    .counters\n                    .stale_feedback\n                    .saturating_add(report.statuses.len() as u64);\n                continue;\n            }\n'''
new = '''            if newest.saturating_sub(base) > MAX_ACK_REORDERING {\n                self.counters.stale_feedback = self\n                    .counters\n                    .stale_feedback\n                    .saturating_add(report.statuses.len() as u64);\n                continue;\n            }\n            let last_reported = base.saturating_add(\n                u64::try_from(report.statuses.len().saturating_sub(1)).unwrap_or(u64::MAX),\n            );\n            if base > newest || last_reported > newest {\n                self.add_unknown(report.statuses.len());\n                continue;\n            }\n'''
if old not in s:
    raise SystemExit("RFC8888 stale block not found")
feedback.write_text(s.replace(old, new, 1))

history_tests = Path("crates/pulsebeam-rtc/src/sent_history/tests.rs")
s = history_tests.read_text()
addition = r'''

#[test]
fn future_twcc_feedback_cannot_advance_ack_edge() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(6);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    let before_bif = history.inputs.bytes_in_flight;
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(20)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Twcc {
            media_ssrc: 7,
            base_sequence: 3,
            reference_time: 0,
            feedback_count: 1,
            statuses: vec![
                TwccStatus::Received { delta_250us: 1 },
                TwccStatus::Received { delta_250us: 1 },
            ]
            .into(),
        },
    });
    assert_eq!(history.inputs.bytes_in_flight, before_bif);
    assert!(history.inputs.feedback.is_empty());
    assert!(history.highest_twcc_acked.is_none());
    assert!(history.counters.unknown_feedback >= 2);
}

#[test]
fn future_rfc8888_feedback_cannot_advance_ack_edge() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(7);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    for sequence in 1..=3 {
        history.commit(rfc_context(now, epoch, sequence)).unwrap();
    }
    let before_bif = history.inputs.bytes_in_flight;
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(20)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Rfc8888 {
            reports: vec![crate::rtcp::Rfc8888Report {
                ssrc: 7,
                begin_sequence: 3,
                report_count: 2,
                statuses: vec![
                    crate::rtcp::Rfc8888Status::Received {
                        ecn: 0,
                        arrival_offset: crate::rtcp::ArrivalOffset::Ticks(1),
                    },
                    crate::rtcp::Rfc8888Status::Received {
                        ecn: 0,
                        arrival_offset: crate::rtcp::ArrivalOffset::Ticks(1),
                    },
                ]
                .into(),
            }]
            .into(),
            report_timestamp: 1,
        },
    });
    assert_eq!(history.inputs.bytes_in_flight, before_bif);
    assert!(history.inputs.feedback.is_empty());
    assert!(history.ssrcs[0].highest_acked_sequence.is_none());
    assert!(history.counters.unknown_feedback >= 2);
}
'''
if "future_twcc_feedback_cannot_advance_ack_edge" not in s:
    s += addition
history_tests.write_text(s)
