use std::{
    fmt::Write as _,
    time::{Duration, Instant},
};

use crate::{
    EcnCodepoint, PacketFeedbackKind,
    congestion::{ControllerInput, EcnValidation, ScreamController},
    connection::TransmitCommitContext,
    receive_feedback::ReceiveFeedback,
    rtcp,
    sent_history::{MAX_EXPIRATIONS_PER_POLL, SentHistory},
    transport::{DatagramKind, PathEpoch, PreparedRtpIdentity, RtpService},
};

fn at(origin: Instant, millis: u64) -> Instant {
    origin.checked_add(Duration::from_millis(millis)).unwrap()
}

fn checkpoint(
    trace: &mut String,
    label: &str,
    origin: Instant,
    millis: u64,
    receiver: &ReceiveFeedback,
    history: &mut SentHistory,
    controller: &mut ScreamController,
) {
    let now = at(origin, millis);
    let inputs = history.controller_inputs();
    writeln!(
        trace,
        "{label} at={millis} flight={} fresh={} exhausted={}",
        inputs.bytes_in_flight, inputs.fresh_network_feedback, inputs.exhausted
    )
    .unwrap();
    let samples = inputs.feedback.iter().chain(&inputs.synthetic).map(|feedback| {
        writeln!(trace, "  packet={:?} ssrc={} service={:?} ledger_lost={} first_loss={} recovery={} origin={:?} receiver={:?} sample={:?}", feedback.sent_id, feedback.ssrc, feedback.service, feedback.lost, feedback.reported_lost_for_the_first_time, feedback.recovered, feedback.loss_origin, feedback.receiver_arrival, feedback.controller_sample(now, origin)).unwrap();
        feedback.controller_sample(now, origin)
    }).collect::<Vec<_>>();
    let timing = inputs.timing.map(|timing| {
        (
            timing.received_at.saturating_duration_since(origin),
            timing.feedback_hold,
            timing.newest_send_age,
        )
    });
    writeln!(
        trace,
        "  timing={timing:?} counters={:?}",
        history.counters()
    )
    .unwrap();
    let envelope = controller.update(
        Duration::from_millis(millis),
        ControllerInput {
            path_epoch: Some(1),
            path_available: true,
            feedback: &samples,
            feedback_hold: inputs
                .timing
                .and_then(|timing| timing.feedback_hold)
                .unwrap_or_default(),
            fresh_network_feedback: inputs.fresh_network_feedback,
            bytes_in_flight: inputs.bytes_in_flight,
            paced_queue_bytes: 0,
            desired_media_rate: 2_000_000,
            ecn: EcnValidation::default(),
        },
    );
    writeln!(trace, "  controller={envelope:?}").unwrap();
    writeln!(
        trace,
        "  deadlines=({:?}, {:?}, {:?}) peak={}",
        receiver
            .deadline()
            .map(|deadline| deadline.saturating_duration_since(origin)),
        history
            .next_deadline(now)
            .map(|deadline| deadline.saturating_duration_since(origin)),
        controller.next_deadline(),
        controller.test_flight_peak()
    )
    .unwrap();
    history.clear_controller_inputs();
}

#[test]
fn feedback_wire_accounting_and_controller_baseline() {
    let mut trace = String::new();
    for mode in [
        PacketFeedbackKind::TransportWide,
        PacketFeedbackKind::Rfc8888,
    ] {
        writeln!(trace, "{mode:?}").unwrap();
        let origin = Instant::now();
        let epoch = PathEpoch::from_value(1);
        let mut receiver = ReceiveFeedback::new(Some(mode), origin);
        receiver.reserve(usize::MAX).unwrap();
        let mut history = SentHistory::new(mode);
        history.path_changed(epoch, true);
        let mut controller = ScreamController::new(2_000_000, Some(1200));
        checkpoint(
            &mut trace,
            "initial",
            origin,
            0,
            &receiver,
            &mut history,
            &mut controller,
        );
        for (offset, sequence) in (65_534_u64..=65_538).enumerate() {
            let wire_sequence = u16::try_from(sequence & 0xffff).unwrap();
            let context = TransmitCommitContext {
                at: at(origin, u64::try_from(offset).unwrap()),
                kind: DatagramKind::Rtp,
                wire_len: 1200,
                path_epoch: Some(epoch),
                rtp: Some(PreparedRtpIdentity {
                    ssrc: 7,
                    sequence: wire_sequence,
                    index: sequence,
                    twcc_sequence: (mode == PacketFeedbackKind::TransportWide)
                        .then_some(wire_sequence),
                    service: RtpService::Original,
                }),
            };
            history.prepare_commit(context).unwrap();
            history.preflight_commit(context).unwrap();
            history.commit_preflighted(context);
            controller.note_send(context.at.saturating_duration_since(origin));
        }
        controller.observe_bytes_in_flight(history.controller_inputs().bytes_in_flight);
        for (millis, sequence, ecn) in [
            (10, 65_534, EcnCodepoint::Ect1),
            (12, 0, EcnCodepoint::Ce),
            (11, 1, EcnCodepoint::Ect0),
        ] {
            receiver.observe(at(origin, millis), 7, sequence, Some(sequence), Some(ecn));
        }
        // The first report is due at 60 ms, but processing is delayed to 70 ms.
        assert!(receiver.poll(at(origin, 59), 9).is_none());
        let bytes = receiver.poll(at(origin, 70), 9).unwrap();
        writeln!(trace, "  wire={bytes:02x?}").unwrap();
        let compound = rtcp::compound(bytes, 9, b"baseline").unwrap();
        let batches = rtcp::parse(&compound, at(origin, 80), epoch, Some(mode))
            .unwrap()
            .feedback;
        assert_eq!(batches.len(), 1);
        for batch in batches {
            history.process_feedback(batch);
        }
        checkpoint(
            &mut trace,
            "report",
            origin,
            80,
            &receiver,
            &mut history,
            &mut controller,
        );
        for batch in rtcp::parse(&compound, at(origin, 81), epoch, Some(mode))
            .unwrap()
            .feedback
        {
            history.process_feedback(batch);
        }
        checkpoint(
            &mut trace,
            "replay",
            origin,
            81,
            &receiver,
            &mut history,
            &mut controller,
        );
        history.expire(at(origin, 120), MAX_EXPIRATIONS_PER_POLL);
        checkpoint(
            &mut trace,
            "reorder-expiry",
            origin,
            120,
            &receiver,
            &mut history,
            &mut controller,
        );
        receiver.observe(
            at(origin, 130),
            7,
            65_535,
            Some(65_535),
            Some(EcnCodepoint::Ect1),
        );
        let bytes = receiver.poll(at(origin, 180), 9).unwrap();
        writeln!(trace, "  wire={bytes:02x?}").unwrap();
        let compound = rtcp::compound(bytes, 9, b"baseline").unwrap();
        for batch in rtcp::parse(&compound, at(origin, 190), epoch, Some(mode))
            .unwrap()
            .feedback
        {
            history.process_feedback(batch);
        }
        checkpoint(
            &mut trace,
            "late-receipt",
            origin,
            190,
            &receiver,
            &mut history,
            &mut controller,
        );
        history.expire(at(origin, 6000), MAX_EXPIRATIONS_PER_POLL);
        checkpoint(
            &mut trace,
            "silence-expiry",
            origin,
            6000,
            &receiver,
            &mut history,
            &mut controller,
        );
        history.expire(at(origin, 6001), MAX_EXPIRATIONS_PER_POLL);
        checkpoint(
            &mut trace,
            "retired",
            origin,
            6001,
            &receiver,
            &mut history,
            &mut controller,
        );
    }
    assert_eq!(
        trace,
        include_str!("../tests/traces/feedback-baseline.trace")
    );
}
