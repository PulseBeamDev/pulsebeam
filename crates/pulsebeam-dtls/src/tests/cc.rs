use super::*;
use pulsebeam_cc::{Config, Controller, EcnValidation, FeedbackSample, Observation, Readiness};

fn observation(feedback: &[FeedbackSample], flight: u64) -> Observation<'_> {
    Observation {
        path_epoch: Some(1),
        path_available: true,
        feedback,
        feedback_hold: Duration::ZERO,
        fresh_network_feedback: !feedback.is_empty(),
        bytes_in_flight: flight,
        paced_queue_bytes: 0,
        desired_media_rate: 1_000_000,
        ecn: EcnValidation::default(),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "reuse the DTLS owner's bounded authenticated-peer pump"
)]
fn send(
    source: &mut Association,
    destination: &mut Association,
    source_trace: &mut Trace,
    destination_trace: &mut Trace,
    controller: &mut Controller,
    origin: Instant,
    now: Duration,
    payload: &[u8],
) -> Result<usize, Error> {
    source.handle_timeout(origin + now)?;
    drain(source, source_trace)?;
    assert!(
        source_trace.packets.is_empty(),
        "handshake/control stays outside CC"
    );
    let before = controller.diagnostics().charged_bytes;
    source.send_application_data(payload)?;
    drain(source, source_trace)?;
    assert_eq!(source_trace.packets.len(), 1);
    let pending = source_trace.packets.pop().unwrap();
    assert!(
        pending.len() > payload.len(),
        "account protected UDP payload, not plaintext"
    );
    let wire = pending.len();
    assert_eq!(
        controller.diagnostics().charged_bytes,
        before,
        "preparation is not acceptance"
    );
    assert_eq!(controller.ready(now, 0, wire), Readiness::Ready);

    // The virtual sink rejects this submission. The same ciphertext stays
    // pending; neither the ledger nor CC has a departure to charge.
    let rejected = pending.clone();
    assert_eq!(controller.ready(now, 0, wire), Readiness::Ready);
    assert_eq!(controller.diagnostics().charged_bytes, before);
    assert_eq!(pending, rejected);

    // Successful sink acceptance is the host's observable commit boundary.
    controller.commit_send(now, wire, u64::try_from(wire).unwrap(), false);
    assert_eq!(
        controller.diagnostics().charged_bytes,
        before + wire as u128
    );
    let receipt_at = now + Duration::from_millis(10);
    destination.handle_timeout(origin + receipt_at)?;
    drain(destination, destination_trace)?;
    let applications = destination_trace.application.len();
    destination.handle_packet(&pending)?;
    drain(destination, destination_trace)?;
    assert_eq!(&destination_trace.application[applications], payload);
    assert_eq!(destination_trace.application.len(), applications + 1);

    // Only authenticated receipt retires flight. Feedback is test-host evidence,
    // not a production feedback codec or a physical UDP-send timestamp.
    let sample = FeedbackSample {
        sent_at: now,
        received_at: receipt_at,
        transport_bytes: u32::try_from(wire).unwrap(),
        received: true,
        newly_acked: true,
        lost: false,
        recovered: false,
        receiver_arrival_micros: Some(i64::try_from(now.as_micros()).unwrap() - 1_000_000),
        ecn: None,
    };
    controller
        .update(receipt_at, observation(&[sample], 0))
        .unwrap();
    assert_eq!(
        controller.diagnostics().charged_bytes,
        before + wire as u128
    );
    assert!(controller.diagnostics().flight_peak >= u64::try_from(wire).unwrap());
    Ok(wire)
}

#[test]
fn protected_datagrams_charge_only_accepted_ciphertext_in_each_direction() -> Result<(), Error> {
    let client_identity = LocalIdentity::generate()?;
    let server_identity = LocalIdentity::generate()?;
    let mut now = test_start();
    let mut client = Association::new(
        Role::Client,
        &client_identity,
        server_identity.fingerprint(),
        4096,
        now,
    )?;
    let mut server = Association::new(
        Role::Server,
        &server_identity,
        client_identity.fingerprint(),
        4096,
        now,
    )?;
    let (mut client_trace, mut server_trace) = (Trace::default(), Trace::default());
    advance(
        &mut client,
        &mut server,
        &mut client_trace,
        &mut server_trace,
        now,
    )?;
    drive(
        &mut client,
        &mut server,
        &mut client_trace,
        &mut server_trace,
        &mut now,
    )?;
    assert_eq!((client_trace.connected, server_trace.connected), (1, 1));
    assert!(client_trace.packets.is_empty() && server_trace.packets.is_empty());
    let origin = now;
    let mut forward = Controller::new(Config::default()).unwrap();
    let mut reverse = Controller::new(Config::default()).unwrap();
    forward.update(Duration::ZERO, observation(&[], 0)).unwrap();
    reverse.update(Duration::ZERO, observation(&[], 0)).unwrap();
    let mut forward_wire = 0;
    for (index, bytes) in [64, 512, 1200].into_iter().enumerate() {
        let at = Duration::from_millis(u64::try_from(index).unwrap() * 100);
        forward.process_idle(at);
        forward_wire += send(
            &mut client,
            &mut server,
            &mut client_trace,
            &mut server_trace,
            &mut forward,
            origin,
            at,
            &vec![0xa5; bytes],
        )?;
    }
    assert_eq!(
        reverse.diagnostics().charged_bytes,
        0,
        "independent directions"
    );
    let at = Duration::from_millis(400);
    reverse.process_idle(at);
    let reverse_wire = send(
        &mut server,
        &mut client,
        &mut server_trace,
        &mut client_trace,
        &mut reverse,
        origin,
        at,
        &[0x5a; 256],
    )?;
    assert_eq!(forward.diagnostics().charged_bytes, forward_wire as u128);
    assert_eq!(reverse.diagnostics().charged_bytes, reverse_wire as u128);
    Ok(())
}
