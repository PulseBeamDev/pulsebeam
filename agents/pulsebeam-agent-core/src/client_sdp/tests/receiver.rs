use super::*;
use pulsebeam_proto::state as wire;

const TWCC: &str = "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";
const OFFSET: &str = "urn:ietf:params:rtp-hdrext:toffset";

fn receiver_context() -> Context {
    let mut capabilities = caps();
    capabilities.extensions.extend([TWCC.into(), OFFSET.into()]);
    Context::new(
        &bootstrap(),
        &remote(),
        capabilities,
        100,
        Limits::default(),
    )
    .unwrap()
}

fn receiving_offer(mid: &str) -> String {
    offer(
        mid,
        "recvonly",
        &format!("a=extmap:2 {TWCC}\r\na=rtcp-rsize\r\n"),
    )
}

fn plan(context: &Context, offer: &str, mid: &str, primary: u32) -> wire::RtpParameters {
    let observation = context.receive_offer(offer, mid).unwrap();
    let mut codec = observation.codecs[0].clone();
    codec.feedback = vec![wire::RtcpFeedback::TransportCc.into()];
    wire::RtpParameters {
        mid: mid.into(),
        codec: Some(codec),
        header_extensions: observation.header_extensions,
        encodings: vec![wire::RtpEncodingParameters {
            ssrc: Some(primary),
            rtx_ssrc: Some(primary.checked_add(1).unwrap()),
            ..Default::default()
        }],
        rtcp: Some(wire::RtcpParameters {
            cname: "server-selected".into(),
            reduced_size: true,
        }),
    }
}

#[test]
fn completed_receive_answer_checks_actual_identities_without_packets() {
    let mut context = receiver_context();
    let local = receiving_offer("receiver");
    let selected = plan(&context, &local, "receiver", 200);
    let candidate = context
        .prepare_receive_plan(&local, owner("receiver"), &selected)
        .unwrap();
    let answer = candidate.answer();
    assert_eq!(
        context.observe_receive_answer(answer, "receiver").unwrap(),
        candidate.bindings().receptions[0]
    );
    for wrong in [
        answer.replace("server-selected", "different-source"),
        answer.replace("a=extmap:2", "a=extmap:3"),
        answer
            .replace("a=ssrc:200 ", "a=ssrc:300 ")
            .replace("a=ssrc-group:FID 200 201", "a=ssrc-group:FID 300 201"),
        local,
    ] {
        assert_ne!(wrong, answer);
        assert!(context.observe_receive_answer(&wrong, "receiver").is_err());
    }
    assert!(context.observe_receive_answer(answer, "other").is_err());
    context
        .finish(candidate.id(), ApplyOutcome::Applied)
        .unwrap();
    assert!(context.observe_receive_answer(answer, "receiver").is_err());
}

#[test]
fn actual_receive_offer_does_not_issue_identities_and_server_plan_survives_cancellation() {
    let context = receiver_context();
    let local = receiving_offer("receiver");
    let observation = context.receive_offer(&local, "receiver").unwrap();
    assert_eq!(observation.codecs.len(), 1);
    assert_eq!(observation.codecs[0].payload_type, Some(96));
    assert_eq!(observation.codecs[0].rtx_payload_type, Some(97));
    assert!(context.applied_bindings().receptions.is_empty());
    let selected = plan(&context, &local, "receiver", 100);
    let allocation = Allocation {
        slot: crate::MediaSlot::RemoteVideo(0),
        format: observation.codecs[0].format.clone(),
    };
    let receiver_owner = allocation.owner();
    let slot = assigned_slot_bindings(&local, vec![(allocation.slot.clone(), "receiver".into())])
        .unwrap()
        .remove(0);
    let mut ids = crate::id::IdGenerator::new();
    let operation = ids.operation();
    let wrong = ids.operation();
    let mut executor = Executor::new(context);
    executor
        .enqueue_allocation(operation, vec![allocation])
        .unwrap();
    assert_eq!(executor.start(), Some(operation));
    assert!(executor.receive_offer(wrong, &local, &slot).is_err());
    let mut wrong_slot = slot.clone();
    wrong_slot.media_index = 0;
    assert!(
        executor
            .receive_offer(operation, &local, &wrong_slot)
            .is_err()
    );
    assert_eq!(
        executor.receive_offer(operation, &local, &slot).unwrap(),
        observation
    );
    let mut zero = selected.clone();
    zero.encodings[0].ssrc = Some(0);
    assert!(
        executor
            .prepare_received(operation, &local, &slot, &zero)
            .is_err()
    );
    let candidate = executor
        .prepare_received(operation, &local, &slot, &selected)
        .unwrap();
    assert!(executor.context_mut().bindings().is_none());
    let binding = &candidate.bindings().receptions[0];
    assert_eq!(binding.encodings[0].primary, Some(100));
    assert_eq!(binding.encodings[0].repair, Some(101));
    assert_eq!(binding.rtcp.cname.as_deref(), Some("server-selected"));
    assert!(binding.rtcp.reduced_size);
    assert_eq!(
        binding.codec.parameters.feedback,
        BTreeSet::from(["transport-cc".into()])
    );
    assert!(!candidate.answer().contains("a=rtcp-fb:96 nack"));
    assert!(!executor.cancel(operation));
    assert_eq!(
        executor.finish(operation, true).unwrap(),
        ApplyOutcome::AppliedButCancelled
    );
    let context = executor.context_mut();
    assert!(context.bindings().is_none());
    assert_eq!(
        context.applied_bindings().receptions[0].encodings[0].primary,
        Some(100)
    );

    let mut retarget = selected.clone();
    retarget.encodings[0].ssrc = Some(102);
    assert!(
        context
            .prepare_receive_plan(&local, receiver_owner, &retarget)
            .is_err()
    );
    let retired = local
        .replace("a=recvonly", "a=inactive")
        .replace("a=bundle-only\r\n", "");
    let candidate = context
        .prepare_retirement(&retired, &["receiver".into()])
        .unwrap();
    context
        .finish(candidate.id(), ApplyOutcome::Applied)
        .unwrap();
    let stopped_section = retired.split_once("m=video").unwrap().1;
    let fresh = receiving_offer("fresh")
        .replace("BUNDLE data fresh", "BUNDLE data receiver fresh")
        .replacen("m=video", &format!("m=video{stopped_section}m=video"), 1);
    let candidate = context
        .prepare_receive_plan(
            &fresh,
            owner("fresh"),
            &wire::RtpParameters {
                mid: "fresh".into(),
                ..selected
            },
        )
        .unwrap_err();
    assert_eq!(candidate, Error::Collision);

    let waiting = receiving_offer("waiting")
        .replace("BUNDLE data waiting", "BUNDLE data receiver waiting")
        .replacen("m=video", &format!("m=video{stopped_section}m=video"), 1);
    let waiting_slot = assigned_slot_bindings(
        &waiting,
        vec![(crate::MediaSlot::RemoteVideo(0), "waiting".into())],
    )
    .unwrap()
    .remove(0);
    let operation = ids.operation();
    executor
        .enqueue_allocation(
            operation,
            vec![Allocation {
                slot: waiting_slot.slot.clone(),
                format: observation.codecs[0].format.clone(),
            }],
        )
        .unwrap();
    assert_eq!(executor.start(), Some(operation));
    executor
        .receive_offer(operation, &waiting, &waiting_slot)
        .unwrap();
    assert!(!executor.cancel(operation));
    assert!(!executor.should_abort(operation));
    assert_eq!(
        executor
            .prepare_received(
                operation,
                &waiting,
                &waiting_slot,
                &wire::RtpParameters::default()
            )
            .unwrap_err(),
        Error::CandidateState
    );
    let stopped = waiting
        .replace("a=recvonly", "a=inactive")
        .replace("a=bundle-only\r\n", "");
    executor
        .prepare_abandoned_allocation(operation, &stopped, &["waiting".into()])
        .unwrap();
    assert_eq!(
        executor.finish(operation, true).unwrap(),
        ApplyOutcome::AppliedButCancelled
    );
    assert!(executor.context_mut().bindings().is_none());
    assert!(
        executor
            .context_mut()
            .applied_bindings()
            .receptions
            .is_empty()
    );
}

#[test]
fn bootstrap_receiver_gets_one_first_plan_without_retargeting_an_active_receiver() {
    let local = receiving_offer("receiver").replace(
        "a=rtcp-rsize",
        &format!("a=extmap:3 {OFFSET}\r\na=rtcp-rsize"),
    );
    let initial = receiver_context()
        .prepare(&local, &[request("receiver", false, true)])
        .unwrap();
    let mut capabilities = caps();
    capabilities.extensions.extend([TWCC.into(), OFFSET.into()]);
    let mut context = Context::new(
        &local,
        initial.answer(),
        capabilities,
        1000,
        Limits::default(),
    )
    .unwrap();
    context
        .adopt_bootstrap_media(&[request("receiver", false, true)])
        .unwrap();
    let old = context.applied_bindings().receptions[0].encodings[0]
        .primary
        .unwrap();
    let unchanged = context.prepare(&local, &[]).unwrap();
    context
        .finish(unchanged.id(), ApplyOutcome::Applied)
        .unwrap();
    let mut selected = plan(&context, &local, "receiver", 900);
    selected
        .header_extensions
        .retain(|extension| extension.uri != OFFSET);
    let slot = assigned_slot_bindings(
        &local,
        vec![(crate::MediaSlot::RemoteVideo(0), "receiver".into())],
    )
    .unwrap()
    .remove(0);
    let receiver_owner = MediaOwner {
        key: crate::rtc_preparation::bootstrap_media_key(&slot.slot),
        group: None,
    };
    let mut executor = Executor::new(context);
    let mut ids = crate::id::IdGenerator::new();
    let rejected = ids.operation();
    let mut inflated = slot.clone();
    inflated.mid.reserve(2 * MAX_SDP_BYTES);
    assert_eq!(
        executor.enqueue_reception(rejected, inflated),
        Err(Error::Capacity)
    );
    let operation = ids.operation();
    executor.enqueue_reception(operation, slot.clone()).unwrap();
    assert_eq!(executor.start(), Some(operation));
    assert_eq!(executor.reception_target(operation).unwrap(), Some(&slot));
    let candidate = executor
        .prepare_received(operation, &local, &slot, &selected)
        .unwrap();
    assert_eq!(
        candidate.bindings().receptions[0].encodings[0].primary,
        Some(900)
    );
    assert_eq!(
        executor.finish(operation, true).unwrap(),
        ApplyOutcome::Applied
    );
    let context = executor.context_mut();
    let installed = context.applied_bindings().receptions[0].clone();
    let unchanged = context.prepare(&local, &[]).unwrap();
    assert_eq!(unchanged.bindings().receptions[0], installed);
    assert!(!unchanged.answer().contains(OFFSET));
    context
        .finish(unchanged.id(), ApplyOutcome::Applied)
        .unwrap();
    let missing = local.replace(&format!("a=extmap:2 {TWCC}\r\n"), "");
    assert_eq!(
        context.prepare(&missing, &[]).unwrap_err(),
        Error::Envelope("receiver".into())
    );
    assert_eq!(
        context
            .prepare(&local, &[request("receiver", false, true)])
            .unwrap_err(),
        Error::Envelope("receiver".into())
    );
    assert_eq!(context.applied_bindings().receptions[0], installed);
    let sender = offer(
        "sender",
        "sendonly",
        &format!("a=ssrc:{old} cname:sender\r\n"),
    );
    let sender_section = sender.split_once("m=video").unwrap().1;
    let conflicting = format!(
        "{}m=video{sender_section}",
        local.replace("BUNDLE data receiver", "BUNDLE data receiver sender")
    );
    assert_eq!(
        context
            .prepare(&conflicting, &[request("sender", true, false)])
            .unwrap_err(),
        Error::Collision
    );
    assert_eq!(
        context.bindings().unwrap().receptions[0]
            .rtcp
            .cname
            .as_deref(),
        Some("server-selected")
    );
    let mut changed = selected.clone();
    changed.rtcp.as_mut().unwrap().cname = "different-lifetime".into();
    assert!(
        context
            .prepare_receive_plan(&local, receiver_owner.clone(), &changed)
            .is_err()
    );
    let same = context
        .prepare_receive_plan(&local, receiver_owner, &selected)
        .unwrap();
    context.finish(same.id(), ApplyOutcome::NotApplied).unwrap();
    assert_eq!(
        context.bindings().unwrap().receptions[0].encodings[0].primary,
        Some(900)
    );
}
