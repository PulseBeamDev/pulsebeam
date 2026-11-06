use super::*;

fn observation() -> Observation<'static> {
    Observation {
        path_epoch: Some(1),
        path_available: true,
        feedback: &[],
        feedback_hold: Duration::ZERO,
        fresh_network_feedback: false,
        bytes_in_flight: 0,
        paced_queue_bytes: 0,
        desired_media_rate: 2_000_000,
        ecn: EcnValidation::default(),
    }
}

#[test]
fn partial_window_padding_block_keeps_only_maintenance_deadline() {
    let mut cc = Controller::new(Config::default()).unwrap();
    let window = cc
        .update(Duration::ZERO, observation())
        .unwrap()
        .max_rtp_bytes_in_flight;
    cc.commit_send(Duration::ZERO, 200, window - 1, false);
    assert_eq!(
        cc.ready(Duration::ZERO, window - 1, 200),
        Readiness::CongestionWindow
    );
    assert_eq!(
        cc.next_deadline(Duration::ZERO, None, Some((window - 1, 200))),
        Some(Duration::from_millis(500))
    );
    cc.process_idle(Duration::from_millis(500));
    assert_eq!(
        cc.next_deadline(Duration::from_millis(500), None, Some((window - 1, 200))),
        Some(Duration::from_secs(3))
    );
    cc.process_idle(Duration::from_secs(3));
    assert_eq!(
        cc.next_deadline(Duration::from_secs(3), None, Some((window - 1, 200))),
        Some(Duration::from_secs(6))
    );
}

#[test]
fn failed_padding_generation_spends_no_wire_or_data_service() {
    let mut cc = Controller::new(Config::default()).unwrap();
    cc.update(Duration::ZERO, observation()).unwrap();
    assert_eq!(cc.prepare_padding(Duration::ZERO), Some(187));
    cc.padding_unavailable();
    assert_eq!(cc.diagnostics().charged_bytes, 0);
    assert_eq!(cc.ready(Duration::ZERO, 0, 1200), Readiness::Ready);
    assert_eq!(
        cc.next_deadline(Duration::from_nanos(1), Some((0, 1200)), None),
        Some(Duration::ZERO)
    );
    assert!(
        cc.next_deadline(Duration::ZERO, None, Some((0, 200)))
            .unwrap()
            > Duration::ZERO
    );
}
