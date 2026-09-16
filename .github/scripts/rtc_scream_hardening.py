from pathlib import Path

core = Path("crates/pulsebeam-rtc/src/congestion/screamv2.rs")
s = core.read_text()
old = """        self.max_bytes_in_flight = self.max_bytes_in_flight.max(input.bytes_in_flight);
        self.ecn_mode = input.ecn_mode;
        self.consume_feedback(now, input);
        self.update_qdelay_filter(now);
        self.reduce_ref_wnd(now, input);
        // RFC 8298bis updates the reference window from newly received feedback.
        // Do not consume accumulated ACK credit from an idle timer poll; this also
        // prevents evidence-free growth while feedback is stale.
        if !input.feedback.is_empty() {
            self.increase_ref_wnd(now, input.target_bitrate_max);
        }
        self.ref_wnd = self.ref_wnd.min(self.max_policed_ref_wnd);
        self.adjust_qdelay_target();
        self.derive_target(input.target_bitrate_max);
"""
new = """        self.max_bytes_in_flight = self.max_bytes_in_flight.max(input.bytes_in_flight);
        self.ecn_mode = input.ecn_mode;
        let has_feedback = !input.feedback.is_empty();
        let has_received = input.feedback.iter().any(|sample| sample.received);
        self.consume_feedback(now, input);
        // The draft updates queue-delay state from received acknowledgements and
        // reference-window state from feedback. Timer-only polls must not replay
        // stale delay evidence or consume accumulated ACK credit.
        if has_received {
            self.update_qdelay_filter(now);
        }
        if has_feedback {
            self.reduce_ref_wnd(now, input);
            self.increase_ref_wnd(now, input.target_bitrate_max);
            if has_received {
                self.adjust_qdelay_target();
            }
        }
        self.ref_wnd = self.ref_wnd.min(self.max_policed_ref_wnd);
        self.derive_target(input.target_bitrate_max);
"""
if old not in s:
    raise SystemExit("SCReAM update block not found")
s = s.replace(old, new, 1)
old_cast = "receiver_arrival_micros: received.then_some((at as i64) * 1_000 - 25_000),"
new_cast = """receiver_arrival_micros: received.then_some(
                i64::try_from(at)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000)
                    .saturating_sub(25_000),
            ),"""
if old_cast not in s:
    raise SystemExit("test cast not found")
s = s.replace(old_cast, new_cast, 1)
marker = """    #[test]
    fn inflection_point_updates_once_until_window_grows() {"""
test = """    #[test]
    fn timer_only_poll_does_not_replay_stale_delay_evidence() {
        let mut cc = ScreamV2::new(4_000_000, None);
        cc.ref_wnd = 20_000;
        cc.qdelay = Duration::from_millis(60);
        cc.qdelay_avg = Duration::from_millis(60);
        let before_window = cc.ref_wnd;
        let before_target = cc.qdelay_target;
        cc.update(Duration::from_millis(100), input(&[], 4_000_000));
        assert_eq!(cc.ref_wnd, before_window);
        assert_eq!(cc.qdelay_target, before_target);
    }

"""
if marker not in s:
    raise SystemExit("core test marker not found")
s = s.replace(marker, test + marker, 1)
core.write_text(s)

scenario = Path("crates/pulsebeam-rtc/src/congestion/scenario.rs")
s = scenario.read_text()
s = s.replace("    pub(crate) probe_overhead_percent: u64,\n", "")
s = s.replace("    pub(crate) duplicate_status_consumptions: u64,\n", "")
s = s.replace("            probe_overhead_percent: 0,\n", "")
s = s.replace("            duplicate_status_consumptions: 0,\n", "")
old = """                    received: packet.received,
                    newly_acked: packet.received,
                    lost: !packet.received,
"""
new = """                    received: packet.received,
                    // Covered sequence-space gaps advance SCReAM's ACK edge too;
                    // confirmed loss is a separate signal.
                    newly_acked: true,
                    lost: !packet.received,
"""
if old not in s:
    raise SystemExit("scenario feedback semantics not found")
scenario.write_text(s.replace(old, new, 1))

scenario_test = Path("crates/pulsebeam-rtc/tests/scream_scenarios.rs")
s = scenario_test.read_text()
s = s.replace("        assert!(metrics.probe_overhead_percent <= 5);\n", "")
s = s.replace("        assert_eq!(metrics.duplicate_status_consumptions, 0);\n", "")
scenario_test.write_text(s)

history_tests = Path("crates/pulsebeam-rtc/src/sent_history/tests.rs")
s = history_tests.read_text()
addition = r'''

#[test]
fn stale_twcc_generation_is_rejected_without_advancing_ack_state() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(4);
    let mut history = SentHistory::new(PacketFeedbackKind::TransportWide);
    history.path_changed(epoch, true);
    for sequence in 1..=5_000_u16 {
        history.commit(context(now, epoch, sequence)).unwrap();
    }
    let bytes_in_flight = history.inputs.bytes_in_flight;
    history.process_feedback(batch(
        now + Duration::from_millis(20),
        epoch,
        vec![TwccStatus::Received { delta_250us: 1 }],
    ));
    assert_eq!(history.counters.stale_feedback, 1);
    assert_eq!(history.inputs.bytes_in_flight, bytes_in_flight);
    assert!(history.inputs.feedback.is_empty());
    assert!(history.highest_twcc_acked.is_none());
}

fn rfc_context(at: Instant, epoch: PathEpoch, sequence: u16) -> TransmitCommitContext {
    TransmitCommitContext {
        at,
        kind: DatagramKind::Rtp,
        wire_len: 1_200,
        path_epoch: Some(epoch),
        rtp: Some(PreparedRtpIdentity {
            ssrc: 7,
            sequence,
            twcc_sequence: None,
            service: RtpService::Original,
        }),
    }
}

#[test]
fn stale_rfc8888_generation_is_rejected_per_ssrc() {
    let now = Instant::now();
    let epoch = PathEpoch::from_value(5);
    let mut history = SentHistory::new(PacketFeedbackKind::Rfc8888);
    history.path_changed(epoch, true);
    for sequence in 1..=5_000_u16 {
        history.commit(rfc_context(now, epoch, sequence)).unwrap();
    }
    let bytes_in_flight = history.inputs.bytes_in_flight;
    history.process_feedback(FeedbackBatch {
        received_at: at(now + Duration::from_millis(20)),
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Rfc8888 {
            reports: vec![crate::rtcp::Rfc8888Report {
                ssrc: 7,
                begin_sequence: 1,
                report_count: 1,
                statuses: vec![crate::rtcp::Rfc8888Status::Received {
                    ecn: 0,
                    arrival_offset: crate::rtcp::ArrivalOffset::Ticks(1),
                }]
                .into(),
            }]
            .into(),
            report_timestamp: 1,
        },
    });
    assert_eq!(history.counters.stale_feedback, 1);
    assert_eq!(history.inputs.bytes_in_flight, bytes_in_flight);
    assert!(history.inputs.feedback.is_empty());
    assert!(history.ssrcs[0].highest_acked_sequence.is_none());
}
'''
if "stale_twcc_generation_is_rejected_without_advancing_ack_state" not in s:
    s += addition
history_tests.write_text(s)
