use super::*;

const MID: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";
fn transport() -> &'static str {
    "a=ice-ufrag:local\r\na=ice-pwd:localpasswordlocalpassword\r\na=fingerprint:sha-256 AA:BB:AA:BB:AA:BB:AA:BB:AA:BB:AA:BB:AA:BB:AA:BB:AA:BB:AA:BB:AA:BB:AA:BB:AA:BB:AA:BB:AA:BB:AA:BB\r\na=setup:actpass\r\n"
}
fn bootstrap() -> String {
    format!(
        "v=0\r\no=- 7 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE data\r\n{}m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:data\r\na=sctp-port:5000\r\na=max-message-size:65536\r\n",
        transport()
    )
}
fn remote() -> String {
    bootstrap()
        .replace("local", "remote")
        .replace("actpass", "passive")
}
fn caps() -> Capabilities {
    Capabilities {
        codecs: vec![CodecCapability {
            repair: true,
            parameters: CodecParameters {
                name: "vp8".into(),
                clock_rate: 90000,
                channels: None,
                fmtp: BTreeMap::new(),
                feedback: ["nack".into(), "transport-cc".into()].into_iter().collect(),
            },
        }],
        extensions: [MID.to_owned()].into_iter().collect(),
        rtcp_modes: [RtcpMode::Compound, RtcpMode::ReducedSize]
            .into_iter()
            .collect(),
    }
}
fn offer(mid: &str, direction: &str, attributes: &str) -> String {
    format!(
        "{}m=video 0 UDP/TLS/RTP/SAVPF 96 97\r\na=bundle-only\r\na=mid:{mid}\r\na={direction}\r\na=rtcp-mux\r\na=rtpmap:96 VP8/90000\r\na=rtpmap:97 rtx/90000\r\na=fmtp:97 apt=96\r\na=rtcp-fb:96 nack\r\na=rtcp-fb:96 transport-cc\r\na=extmap:1 {MID}\r\n{attributes}",
        bootstrap().replace("BUNDLE data", &format!("BUNDLE data {mid}"))
    )
}
fn owner(key: &str) -> MediaOwner {
    MediaOwner {
        key: MediaKey(key.into()),
        group: Some(MediaKey("group".into())),
    }
}
fn request(mid: &str, publication: bool, reception: bool) -> Preparation {
    Preparation {
        mid: mid.into(),
        payload_type: 96,
        publication: publication.then(|| owner("pub")),
        reception: reception.then(|| owner("recv")),
    }
}
fn context() -> Context {
    Context::new(&bootstrap(), &remote(), caps(), 100, Limits::default()).unwrap()
}

#[test]
fn executor_serializes_real_context_history_and_reports_cancellation_truthfully() {
    let mut ids = crate::id::IdGenerator::new();
    let first = ids.operation();
    let second = ids.operation();
    let third = ids.operation();
    let mut executor = Executor::new(context());
    executor.enqueue(first, vec![]).unwrap();
    executor.enqueue(second, vec![]).unwrap();
    executor.enqueue(third, vec![]).unwrap();
    assert_eq!(executor.start(), Some(first));
    assert_eq!(executor.start(), None);
    let mut applied = executor.prepare(first, &bootstrap()).unwrap();
    assert_eq!(
        executor.prepare_sender_modes(second, &mut applied, "v", &[]),
        Err(Error::CandidateState)
    );
    assert_eq!(
        executor.prepare_sender_modes(first, &mut applied, "v", &[]),
        Err(Error::Observation)
    );
    assert!(!executor.cancel(first));
    assert_eq!(
        executor.prepare_sender_modes(first, &mut applied, "v", &[]),
        Err(Error::CandidateState)
    );
    assert!(executor.cancelled(first));
    assert_eq!(executor.finish(second, true), Err(Error::CandidateState));
    assert_eq!(
        executor.finish(first, true),
        Ok(ApplyOutcome::AppliedButCancelled)
    );
    assert!(executor.context_mut().bindings().is_none());
    assert_eq!(executor.start(), Some(second));
    assert_eq!(executor.finish(first, false), Err(Error::CandidateState));
    let continuing = executor.prepare(second, &bootstrap()).unwrap();
    assert_ne!(continuing.answer(), applied.answer());
    assert_eq!(executor.finish(second, true), Ok(ApplyOutcome::Applied));
    assert!(executor.context_mut().bindings().is_some());
    assert_eq!(executor.start(), Some(third));
    assert!(!executor.cancel(third));
    assert!(executor.should_abort(third));
    assert_eq!(
        executor.prepare(third, &bootstrap()).unwrap_err(),
        Error::CandidateState
    );
    assert_eq!(executor.finish(third, false), Ok(ApplyOutcome::NotApplied));
    assert!(executor.context_mut().bindings().is_some());
    assert!(executor.enqueue(first, vec![]).is_err());
    let live = ids.operation();
    executor
        .enqueue(live, vec![request("old", false, true)])
        .unwrap();
    assert_eq!(executor.start(), Some(live));
    let active = offer("old", "recvonly", "");
    executor.prepare(live, &active).unwrap();
    executor.finish(live, true).unwrap();
    let queued = ids.operation();
    executor
        .enqueue_retirement(queued, vec!["old".into()])
        .unwrap();
    assert!(executor.cancel(queued));
    assert_eq!(
        executor.context_mut().bindings().unwrap().receptions.len(),
        1
    );
    for mid in ["data", "unknown"] {
        let invalid = ids.operation();
        executor
            .enqueue_retirement(invalid, vec![mid.into()])
            .unwrap();
        assert_eq!(executor.start(), Some(invalid));
        assert_eq!(
            executor.retirement_targets(invalid).unwrap_err(),
            Error::Envelope(mid.into())
        );
        assert_eq!(
            executor.finish(invalid, false),
            Ok(ApplyOutcome::NotApplied)
        );
        assert_eq!(
            executor.context_mut().bindings().unwrap().receptions.len(),
            1
        );
    }
    let refused = ids.operation();
    executor
        .enqueue_retirement(refused, vec!["old".into()])
        .unwrap();
    assert_eq!(executor.start(), Some(refused));
    assert_eq!(executor.retirement_targets(refused).unwrap(), ["old"]);
    assert_eq!(
        executor.prepare(refused, &active).unwrap_err(),
        Error::Envelope("old".into())
    );
    assert_eq!(
        executor.finish(refused, false),
        Ok(ApplyOutcome::NotApplied)
    );
    assert_eq!(
        executor.context_mut().bindings().unwrap().receptions.len(),
        1
    );
    let retirement = ids.operation();
    executor
        .enqueue_retirement(retirement, vec!["old".into()])
        .unwrap();
    assert_eq!(executor.start(), Some(retirement));
    let stopped = offer("old", "inactive", "").replace("a=bundle-only\r\n", "");
    assert_eq!(executor.retirement_targets(retirement).unwrap(), ["old"]);
    assert!(!executor.cancel(retirement));
    assert!(executor.cancelled(retirement));
    assert!(!executor.should_abort(retirement));
    assert_eq!(
        executor.retirement_targets(retirement),
        Err(Error::CandidateState)
    );
    let candidate = executor.prepare(retirement, &stopped).unwrap();
    assert!(candidate.bindings().receptions.is_empty());
    assert_eq!(
        executor.finish(retirement, true),
        Ok(ApplyOutcome::AppliedButCancelled)
    );
    assert!(executor.context_mut().bindings().is_none());
    let next = ids.operation();
    executor.enqueue(next, vec![]).unwrap();
    assert_eq!(executor.start(), Some(next));
    let continued = executor.prepare(next, &stopped).unwrap();
    assert!(continued.bindings().receptions.is_empty());
    executor.finish(next, true).unwrap();
    assert!(
        executor
            .context_mut()
            .bindings()
            .unwrap()
            .receptions
            .is_empty()
    );
}

#[test]
fn executor_bounds_unfinished_work_and_reclaims_cancelled_queue_credit() {
    let mut ids = crate::id::IdGenerator::new();
    let mut executor = Executor::new(context());
    let first = ids.operation();
    executor.enqueue(first, vec![]).unwrap();
    for _ in 1..32 {
        executor.enqueue(ids.operation(), vec![]).unwrap();
    }
    assert_eq!(
        executor.enqueue(ids.operation(), vec![]),
        Err(Error::Capacity)
    );
    let allocation = Allocation {
        slot: crate::MediaSlot::LocalVideo("camera".into()),
        format: None,
    };
    assert_eq!(
        executor.enqueue_allocation(ids.operation(), vec![allocation.clone()]),
        Err(Error::Capacity)
    );
    assert!(executor.cancel(first));
    executor
        .enqueue_allocation(ids.operation(), vec![allocation])
        .unwrap();
    assert!(executor.ready());
    assert_eq!(
        executor.start().map(|operation| operation == first),
        Some(false)
    );
    let mut executor = Executor::new(context());
    let mut name = String::with_capacity(2 * MAX_SDP_BYTES);
    name.push_str("camera");
    let allocation = Allocation {
        slot: crate::MediaSlot::LocalVideo(name),
        format: None,
    };
    assert_eq!(
        executor.enqueue_allocation(ids.operation(), vec![allocation]),
        Err(Error::Capacity)
    );
    assert!(!executor.ready());
    for field in 0..4 {
        use pulsebeam_proto::state::{CodecFormat, CodecParameter, Vp8Parameters, codec_format};
        let mut format = CodecFormat::from(codec_format::Codec::Vp8(Vp8Parameters {}));
        if field == 0 {
            format.mime_type.reserve(2 * MAX_SDP_BYTES);
        } else if field == 1 {
            format
                .parameters
                .reserve(2 * MAX_SDP_BYTES / size_of::<CodecParameter>() + 1);
        } else {
            let mut parameter = CodecParameter {
                name: "hint".into(),
                value: "1".into(),
            };
            if field == 2 {
                parameter.name.reserve(2 * MAX_SDP_BYTES);
            } else {
                parameter.value.reserve(2 * MAX_SDP_BYTES);
            }
            format.parameters.push(parameter);
        }
        assert_eq!(
            executor.enqueue_allocation(
                ids.operation(),
                vec![Allocation {
                    slot: crate::MediaSlot::RemoteVideo(0),
                    format: Some(format),
                }]
            ),
            Err(Error::Capacity)
        );
        assert!(!executor.ready());
    }
    let mut preparation = request("v", true, false);
    preparation.mid.reserve(2 * MAX_SDP_BYTES);
    assert_eq!(
        executor.enqueue(ids.operation(), vec![preparation]),
        Err(Error::Capacity)
    );
    assert!(!executor.ready());
    let mut allocations = Vec::with_capacity(2 * MAX_SDP_BYTES / size_of::<Allocation>() + 1);
    let mut mids = Vec::with_capacity(2 * MAX_SDP_BYTES / size_of::<String>() + 1);
    for index in 0..257 {
        allocations.push(Allocation {
            slot: crate::MediaSlot::LocalVideo(format!("camera{index}")),
            format: None,
        });
        mids.push(format!("v{index}"));
    }
    let rejected = ids.operation();
    assert_eq!(
        executor.enqueue_allocation(rejected, allocations),
        Err(Error::Capacity)
    );
    assert_eq!(
        executor.enqueue(rejected, vec![]),
        Err(Error::CandidateState)
    );
    assert_eq!(
        executor.enqueue_retirement(ids.operation(), mids),
        Err(Error::Capacity)
    );
    assert!(!executor.ready());
    let invalid = Allocation {
        slot: crate::MediaSlot::RemoteVideo(0),
        format: None,
    };
    assert_eq!(
        executor.enqueue_allocation(ids.operation(), vec![invalid]),
        Err(Error::Identifier)
    );
}

#[test]
fn allocation_lowers_only_actual_sections_and_reconciles_cancelled_physical_history() {
    use crate::{MediaDirection, MediaKind, MediaSlot};
    use pulsebeam_proto::state::{CodecFormat, codec_format};

    let publication = Allocation {
        slot: MediaSlot::LocalVideo("camera".into()),
        format: None,
    };
    let reception = Allocation {
        slot: MediaSlot::RemoteVideo(7),
        format: Some(CodecFormat::from(codec_format::Codec::Vp8(
            Default::default(),
        ))),
    };
    let requests = vec![publication.clone(), reception.clone()];
    let send = offer(
        "physical-send",
        "sendonly",
        "a=ssrc:0 cname:native\r\na=ssrc:4294967295 cname:native\r\na=ssrc-group:FID 0 4294967295\r\n",
    );
    let recv = offer("physical-recv", "recvonly", "");
    let actual = format!("{send}m=video{}", recv.split_once("m=video").unwrap().1)
        .replace(
            "BUNDLE data physical-send",
            "BUNDLE data physical-send physical-recv",
        )
        .replace(" 96 97", " 120 121")
        .replace(":96", ":120")
        .replace(":97", ":121")
        .replace("apt=96", "apt=120");
    let slots = assigned_slot_bindings(
        &actual,
        [
            (publication.slot.clone(), "physical-send".into()),
            (reception.slot.clone(), "physical-recv".into()),
        ],
    )
    .unwrap();
    let mut ids = crate::id::IdGenerator::new();
    let mut executor = Executor::new(context());
    let operation = ids.operation();
    executor
        .enqueue_allocation(operation, requests.clone())
        .unwrap();
    assert_eq!(executor.start(), Some(operation));
    assert_eq!(executor.allocation_targets(operation).unwrap(), requests);
    assert_eq!(
        executor.prepare(operation, &actual).unwrap_err(),
        Error::CandidateState
    );
    for invalid in 0..4 {
        let mut corrupted = slots.clone();
        match invalid {
            0 => corrupted[0].mid = "guessed".into(),
            1 => corrupted[0].media_index = 0,
            2 => corrupted[0].kind = MediaKind::Audio,
            _ => corrupted[0].direction = MediaDirection::ReceiveOnly,
        }
        assert!(
            executor
                .prepare_allocated(operation, &actual, &corrupted)
                .is_err()
        );
        assert!(executor.context_mut().bindings().is_none());
    }
    assert!(!executor.cancel(operation));
    assert!(!executor.should_abort(operation));
    assert_eq!(
        executor.allocation_targets(operation).unwrap_err(),
        Error::CandidateState
    );
    let candidate = executor
        .prepare_allocated(operation, &actual, &slots)
        .unwrap();
    let p = &candidate.bindings().publications[0];
    let r = &candidate.bindings().receptions[0];
    assert_eq!(p.mid, "physical-send");
    assert_eq!(r.mid, "physical-recv");
    assert_eq!(p.key, publication.owner().key);
    assert_eq!(r.key, reception.owner().key);
    assert_eq!(p.codec.payload_type, 120);
    assert_eq!(r.codec.payload_type, 120);
    assert_eq!(r.codec.repair.as_ref().unwrap().payload_type, 121);
    assert_eq!(p.encodings[0].primary, Some(0));
    assert_eq!(p.encodings[0].repair, Some(u32::MAX));
    assert_ne!(p.encodings[0].primary, r.encodings[0].primary);
    assert_ne!(p.encodings[0].repair, r.encodings[0].repair);
    assert!(p.complete() && r.complete());
    let old_receive_identity = r.encodings[0].primary;
    let old_receive_repair = r.encodings[0].repair;
    let old_msids = candidate
        .answer()
        .lines()
        .filter(|line| line.starts_with("a=msid:"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(!old_msids.is_empty());
    assert_eq!(
        executor.finish(operation, true),
        Ok(ApplyOutcome::AppliedButCancelled)
    );
    assert!(executor.context_mut().bindings().is_none());

    let occupied = ids.operation();
    executor.enqueue_allocation(occupied, requests).unwrap();
    executor.start().unwrap();
    assert_eq!(
        executor.allocation_targets(occupied).unwrap_err(),
        Error::Envelope("physical-send".into())
    );
    assert!(
        executor
            .prepare_allocated(occupied, &actual, &slots)
            .is_err()
    );
    executor.finish(occupied, false).unwrap();
    assert!(executor.context_mut().bindings().is_none());

    let occupied_receiver = ids.operation();
    executor
        .enqueue_allocation(occupied_receiver, vec![reception.clone()])
        .unwrap();
    executor.start().unwrap();
    assert_eq!(
        executor.allocation_targets(occupied_receiver).unwrap_err(),
        Error::Envelope("physical-recv".into())
    );
    executor.finish(occupied_receiver, false).unwrap();

    let retirement = ids.operation();
    executor
        .enqueue_retirement(
            retirement,
            slots.iter().map(|slot| slot.mid.clone()).collect(),
        )
        .unwrap();
    executor.start().unwrap();
    assert_eq!(executor.retirement_targets(retirement).unwrap().len(), 2);
    let stopped = actual
        .replace("a=bundle-only\r\n", "")
        .replace("a=sendonly", "a=inactive")
        .replace("a=recvonly", "a=inactive");
    executor.prepare(retirement, &stopped).unwrap();
    executor.finish(retirement, true).unwrap();
    assert_eq!(
        executor.context_mut().bindings(),
        Some(&Bindings::default())
    );

    let replacement = ids.operation();
    let fresh = offer("physical-replacement", "recvonly", "");
    let assigned = assigned_slot_bindings(
        &fresh,
        [(reception.slot.clone(), "physical-replacement".into())],
    )
    .unwrap();
    executor
        .enqueue_allocation(replacement, vec![reception])
        .unwrap();
    executor.start().unwrap();
    let next = executor
        .prepare_allocated(replacement, &fresh, &assigned)
        .unwrap();
    let r = &next.bindings().receptions[0];
    assert_eq!(r.mid, "physical-replacement");
    assert_eq!(r.codec.payload_type, 96);
    assert_ne!(r.encodings[0].primary, old_receive_identity);
    assert_ne!(r.encodings[0].repair, old_receive_repair);
    assert_ne!(
        next.answer()
            .lines()
            .filter(|line| line.starts_with("a=msid:"))
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        old_msids
    );
    assert!(r.complete());
    executor.finish(replacement, true).unwrap();
    assert_eq!(
        executor.context_mut().bindings().unwrap().receptions.len(),
        1
    );
    let preserved = executor.context_mut().bindings().unwrap().receptions[0].clone();
    let failed = ids.operation();
    let pub_slot = crate::MediaSlot::LocalVideo("failed".into());
    let extra = offer(
        "abandoned",
        "sendonly",
        "a=ssrc:70 cname:native\r\na=ssrc:71 cname:native\r\na=ssrc-group:FID 70 71\r\n",
    );
    let expanded = format!("{fresh}m=video{}", extra.split_once("m=video").unwrap().1).replace(
        "BUNDLE data physical-replacement",
        "BUNDLE data physical-replacement abandoned",
    );
    let assigned =
        assigned_slot_bindings(&expanded, [(pub_slot.clone(), "abandoned".into())]).unwrap();
    executor
        .enqueue_allocation(
            failed,
            vec![Allocation {
                slot: pub_slot,
                format: None,
            }],
        )
        .unwrap();
    executor.start().unwrap();
    executor
        .prepare_allocated(failed, &expanded, &assigned)
        .unwrap();
    assert!(
        executor
            .prepare_abandoned_allocation(failed, &expanded, &["abandoned".into()])
            .is_err()
    );
    let cleanup = extra
        .split_once("m=video")
        .unwrap()
        .1
        .replace("a=bundle-only\r\n", "")
        .replace("a=sendonly", "a=inactive");
    let cleanup = format!("{fresh}m=video{cleanup}").replace(
        "BUNDLE data physical-replacement",
        "BUNDLE data physical-replacement abandoned",
    );
    let candidate = executor
        .prepare_abandoned_allocation(failed, &cleanup, &["abandoned".into()])
        .unwrap();
    assert!(candidate.bindings().publications.is_empty());
    assert_eq!(candidate.bindings().receptions, [preserved.clone()]);
    assert_eq!(
        executor.finish(failed, true),
        Ok(ApplyOutcome::AppliedButCancelled)
    );
    let context = executor.context_mut();
    assert!(context.bindings().is_none());
    assert!(context.stopped("abandoned"));
    assert!(!context.stopped("physical-replacement"));
    assert_eq!(context.applied_bindings().receptions, [preserved]);
}

#[test]
fn bootstrap_capabilities_preserve_per_codec_repair_and_rtcp_modes() {
    use pulsebeam_proto::state::{self as wire, codec_format};
    let capability = |codec, repair| wire::RtpCodecCapability {
        format: Some(wire::CodecFormat::from(codec)),
        feedback: vec![
            wire::RtcpFeedback::Nack.as_str().into(),
            wire::RtcpFeedback::TransportCc.as_str().into(),
        ],
        rtx_supported: repair,
        ..Default::default()
    };
    let mut envelope = wire::RtpCapabilities {
        codecs: vec![
            capability(codec_format::Codec::Vp8(Default::default()), true),
            capability(codec_format::Codec::Vp9(Default::default()), false),
        ],
        header_extensions: vec![
            wire::RtpHeaderExtensionType::RtpHeaderExtensionMid
                .as_str()
                .into(),
        ],
        rtcp_modes: vec![wire::RtcpMode::Compound.as_str().into()],
    };
    envelope.codecs.push(wire::RtpCodecCapability {
        format: Some(wire::CodecFormat {
            mime_type: "video/future-codec".into(),
            clock_rate: 90000,
            parameters: vec![wire::CodecParameter {
                name: "future-hint".into(),
                value: "1".into(),
            }],
            ..Default::default()
        }),
        ..Default::default()
    });
    envelope
        .header_extensions
        .push("urn:future:extension".into());
    envelope.codecs.push(wire::RtpCodecCapability {
        format: Some(wire::CodecFormat {
            mime_type: "audio/future-codec".into(),
            clock_rate: u32::MAX,
            channels: Some(u32::MAX),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert_eq!(Capabilities::from_wire(&envelope).unwrap().codecs.len(), 3);
    envelope.rtcp_modes.push("future-rtcp-mode".into());
    for capability in &mut envelope.codecs {
        capability.feedback.push("future-feedback".into());
    }
    for (index, (name, repair)) in [("VP8", true), ("VP9", false)].into_iter().enumerate() {
        let ctx = Context::new(
            &bootstrap(),
            &remote(),
            Capabilities::from_wire(&envelope).unwrap(),
            100,
            Limits::default(),
        )
        .unwrap();
        let mut actual = offer("v", "recvonly", "").replace("VP8", name);
        if name == "VP9" {
            actual = actual.replace(
                "a=rtpmap:96 VP9/90000\r\n",
                "a=rtpmap:96 VP9/90000\r\na=fmtp:96 profile-id=0\r\n",
            );
        }
        let slot = crate::MediaSlot::RemoteVideo(0);
        let assigned = assigned_slot_bindings(&actual, [(slot.clone(), "v".into())]).unwrap();
        let mut executor = Executor::new(ctx);
        let operation = crate::id::IdGenerator::new().operation();
        executor
            .enqueue_allocation(
                operation,
                vec![Allocation {
                    slot,
                    format: envelope.codecs[index].format.clone(),
                }],
            )
            .unwrap();
        executor.start().unwrap();
        if name == "VP9" {
            let wrong_format = offer("v", "recvonly", "");
            assert!(
                executor
                    .prepare_allocated(operation, &wrong_format, &assigned)
                    .is_err()
            );
            assert!(executor.context_mut().bindings().is_none());
        }
        let candidate = executor
            .prepare_allocated(operation, &actual, &assigned)
            .unwrap();
        let selected = &candidate.bindings().receptions[0];
        assert_eq!(selected.codec.repair.is_some(), repair);
        assert!(
            !selected
                .codec
                .parameters
                .feedback
                .contains("future-feedback")
        );
        assert!(
            selected
                .extensions
                .iter()
                .all(|extension| extension.uri != "urn:future:extension")
        );
    }
    envelope.rtcp_modes = vec![wire::RtcpMode::ReducedSize.as_str().into()];
    let mut ctx = Context::new(
        &bootstrap(),
        &remote(),
        Capabilities::from_wire(&envelope).unwrap(),
        100,
        Limits::default(),
    )
    .unwrap();
    assert!(
        ctx.prepare(&offer("v", "recvonly", ""), &[request("v", false, true)])
            .is_err()
    );
    assert!(ctx.bindings().is_none());
    let empty = Capabilities::from_wire(&wire::RtpCapabilities::default()).unwrap();
    assert!(Context::new(&bootstrap(), &remote(), empty, 0, Limits::default()).is_ok());
}

#[test]
fn data_only_bootstrap_prepares_duplex_with_independent_identities_and_rtp_facts() {
    let mut ctx = context();
    let actual = offer(
        "v",
        "sendrecv",
        "a=ssrc:0 cname:native\r\na=ssrc:4294967295 cname:native\r\na=ssrc-group:FID 0 4294967295\r\n",
    );
    let candidate = ctx.prepare(&actual, &[request("v", true, true)]).unwrap();
    let p = &candidate.bindings().publications[0];
    let r = &candidate.bindings().receptions[0];
    assert!(p.complete() && r.complete());
    assert_eq!(p.encodings[0].primary, Some(0));
    assert_eq!(p.encodings[0].repair, Some(u32::MAX));
    assert_ne!(p.encodings[0].primary, r.encodings[0].primary);
    let answer = inspect(candidate.answer()).unwrap();
    assert_eq!(answer[1].codecs[0], r.codec);
    assert_eq!(p.rtcp.cname.as_deref(), Some("native"));
    assert_eq!(answer[1].rtcp, r.rtcp);
    assert_ne!(p.rtcp.cname, r.rtcp.cname);
    assert!(!r.rtcp.reduced_size);
    assert_eq!(answer[1].encodings, r.encodings);
    assert_eq!(answer[1].mid, "v");
    assert!(candidate.answer().contains("o=- 7 3 IN IP4"));
    ctx.finish(candidate.id(), ApplyOutcome::Applied).unwrap();
    assert_eq!(ctx.bindings(), Some(candidate.bindings()));
    let retained = ctx.prepare(&actual, &[]).unwrap();
    assert_eq!(retained.bindings(), candidate.bindings());
}

#[test]
fn bootstrap_media_is_adopted_once_with_caller_keys_and_retained_without_restating_it() {
    let mut generator = context();
    let actual = offer(
        "v",
        "sendrecv",
        "a=ssrc:4294967295 cname:native\r\na=ssrc:0 cname:native\r\na=ssrc-group:FID 4294967295 0\r\n",
    ).replace("a=rtcp-mux\r\n", "a=rtcp-mux\r\na=rtcp-rsize\r\n");
    let applied = generator
        .prepare(&actual, &[request("v", true, true)])
        .unwrap();
    let encoding = &applied.bindings().publications[0].encodings[0];
    assert_eq!(encoding.primary, Some(u32::MAX));
    assert_eq!(encoding.repair, Some(0));
    let answer = applied
        .answer()
        .replace("a=rtcp-mux\r\n", "a=rtcp-mux\r\na=rtcp-rsize\r\n")
        .replace("cname:pulsebeam-00000065", "cname:bootstrap-sync")
        .replace("cname:pulsebeam-00000066", "cname:bootstrap-sync");
    let mut expected = applied.bindings().clone();
    expected.publications[0].rtcp.reduced_size = true;
    expected.receptions[0].rtcp.reduced_size = true;
    expected.receptions[0].rtcp.cname = Some("bootstrap-sync".into());
    assert!(
        Context::new(
            &actual.replace("a=rtcp-rsize\r\n", ""),
            &answer,
            caps(),
            900,
            Limits::default()
        )
        .is_err()
    );
    let mut ctx = Context::new(&actual, &answer, caps(), 900, Limits::default()).unwrap();
    assert!(matches!(
        ctx.adopt_bootstrap_media(&[]),
        Err(Error::MissingConfiguration(_))
    ));
    assert!(ctx.bindings().is_none());
    assert_eq!(
        ctx.adopt_bootstrap_media(&[request("v", true, true)])
            .unwrap(),
        &expected
    );
    assert_eq!(
        ctx.adopt_bootstrap_media(&[request("v", true, true)]),
        Err(Error::CandidateState)
    );
    let retained = ctx.prepare(&actual, &[]).unwrap();
    assert_eq!(retained.bindings(), &expected);
    assert!(retained.answer().contains("a=rtcp-rsize\r\n"));
    assert!(retained.answer().contains("cname:bootstrap-sync\r\n"));
    assert!(retained.answer().contains("o=- 7 4 IN IP4"));
    let primary = retained.bindings().receptions[0].encodings[0]
        .primary
        .unwrap();
    assert!(
        retained
            .answer()
            .contains(&format!("a=msid:pulsebeam-v output-{primary}\r\n"))
    );
}

#[test]
fn bootstrap_selected_feedback_is_retained_when_capabilities_allow_more() {
    let mut generator = context();
    let actual = offer(
        "v",
        "sendonly",
        "a=ssrc:200 cname:native\r\na=ssrc:201 cname:native\r\na=ssrc-group:FID 200 201\r\n",
    );
    let applied = generator
        .prepare(&actual, &[request("v", true, false)])
        .unwrap();
    let answer = applied
        .answer()
        .replace("a=rtcp-fb:96 transport-cc\r\n", "");
    let mut ctx = Context::new(&actual, &answer, caps(), 900, Limits::default()).unwrap();
    let adopted = ctx
        .adopt_bootstrap_media(&[request("v", true, false)])
        .unwrap()
        .clone();
    assert_eq!(
        adopted.publications[0].codec.parameters.feedback,
        ["nack".into()].into_iter().collect()
    );
    let retained = ctx.prepare(&actual, &[]).unwrap();
    assert_eq!(retained.bindings(), &adopted);
    ctx.finish(retained.id(), ApplyOutcome::Applied).unwrap();
    assert_eq!(
        ctx.prepare(&actual, &[request("v", true, false)])
            .unwrap()
            .bindings(),
        &adopted
    );
}

#[test]
fn failed_application_does_not_advance_origin_and_cancel_after_apply_does() {
    let mut ctx = context();
    let actual = offer("v", "recvonly", "");
    let first = ctx.prepare(&actual, &[request("v", false, true)]).unwrap();
    let first_ssrc = first.bindings().receptions[0].encodings[0].primary;
    assert_eq!(
        ctx.prepare(&actual, &[]).unwrap_err(),
        Error::CandidateState
    );
    ctx.finish(first.id(), ApplyOutcome::NotApplied).unwrap();
    assert!(ctx.bindings().is_none());
    let second = ctx.prepare(&actual, &[request("v", false, true)]).unwrap();
    assert!(second.answer().contains("o=- 7 3 IN IP4"));
    assert_ne!(
        second.bindings().receptions[0].encodings[0].primary,
        first_ssrc
    );
    ctx.finish(second.id(), ApplyOutcome::AppliedButCancelled)
        .unwrap();
    assert!(ctx.bindings().is_none());
    let third = ctx.prepare(&actual, &[]).unwrap();
    assert!(third.answer().contains("o=- 7 4 IN IP4"));
    assert_eq!(third.bindings(), second.bindings());
    assert_eq!(
        ctx.finish(first.id(), ApplyOutcome::Applied),
        Err(Error::CandidateState)
    );
}

#[test]
fn partial_sender_facts_reserve_known_identities_without_claiming_completion() {
    let mut ctx = context();
    let actual = offer(
        "v",
        "sendonly",
        "a=rid:high send\r\na=simulcast:send high\r\n",
    );
    let candidate = ctx.prepare(&actual, &[request("v", true, false)]).unwrap();
    ctx.finish(candidate.id(), ApplyOutcome::Applied).unwrap();
    let mut facts = ctx.bindings().unwrap().publications[0].encodings.clone();
    facts[0].primary = Some(0);
    let observed = ctx
        .observe_sender_facts("v", &facts, &RtcpParameters::default())
        .unwrap()
        .clone();
    assert_eq!(observed.publications[0].encodings[0].primary, Some(0));
    assert!(!observed.publications[0].complete());
    let mut conflicting = facts.clone();
    conflicting[0].primary = Some(7);
    assert_eq!(
        ctx.observe_sender_facts("v", &conflicting, &RtcpParameters::default()),
        Err(Error::Observation)
    );
    assert_eq!(ctx.bindings(), Some(&observed));
    facts[0].repair = Some(1);
    let rtcp = RtcpParameters {
        cname: Some("native".into()),
        reduced_size: false,
    };
    assert!(
        ctx.observe_sender_facts("v", &facts, &rtcp)
            .unwrap()
            .publications[0]
            .complete()
    );
    facts[0].primary = None;
    facts[0].repair = None;
    let retained = ctx
        .observe_sender_facts("v", &facts, &RtcpParameters::default())
        .unwrap();
    assert_eq!(retained.publications[0].encodings[0].primary, Some(0));
    assert_eq!(retained.publications[0].encodings[0].repair, Some(1));
    assert!(retained.publications[0].complete());

    let mut bounded = Context::new(
        &bootstrap(),
        &remote(),
        caps(),
        100,
        Limits {
            max_identity_bytes: identity_history::guard_bytes(1).unwrap()
                + size_of::<(u32, identity_history::SenderOwner)>()
                + "v".len()
                + "high".len(),
        },
    )
    .unwrap();
    let candidate = bounded
        .prepare(&actual, &[request("v", true, false)])
        .unwrap();
    bounded
        .finish(candidate.id(), ApplyOutcome::Applied)
        .unwrap();
    let before = bounded.bindings().unwrap().clone();
    let mut batch = before.publications[0].encodings.clone();
    batch[0].primary = Some(0);
    batch[0].repair = Some(1);
    assert_eq!(
        bounded.observe_sender("v", &batch, &rtcp),
        Err(Error::Capacity)
    );
    assert_eq!(bounded.bindings(), Some(&before));
    batch[0].primary = Some(2);
    batch[0].repair = None;
    let accepted = bounded
        .observe_sender_facts("v", &batch, &RtcpParameters::default())
        .unwrap();
    assert_eq!(accepted.publications[0].encodings[0].primary, Some(2));
    assert!(!accepted.publications[0].complete());
}

#[test]
fn rejected_retired_sender_facts_do_not_consume_fresh_identities() {
    let mut ctx = context();
    let actual = offer(
        "v",
        "sendonly",
        "a=rid:high send\r\na=rid:low send\r\na=simulcast:send high;low\r\n",
    );
    let first = ctx.prepare(&actual, &[request("v", true, false)]).unwrap();
    ctx.finish(first.id(), ApplyOutcome::Applied).unwrap();
    let mut facts = ctx.bindings().unwrap().publications[0].encodings.clone();
    facts[0].primary = Some(0);
    facts[1].primary = Some(2);
    ctx.observe_sender_facts("v", &facts, &RtcpParameters::default())
        .unwrap();
    let stopped = ctx
        .prepare(&actual.replace("a=bundle-only\r\n", ""), &[])
        .unwrap();
    ctx.finish(stopped.id(), ApplyOutcome::Applied).unwrap();
    let fresh = ctx.prepare(&actual, &[request("v", true, false)]).unwrap();
    ctx.finish(fresh.id(), ApplyOutcome::Applied).unwrap();
    let before = ctx.bindings().unwrap().clone();
    facts[0].primary = Some(101);
    assert_eq!(
        ctx.observe_sender_facts("v", &facts, &RtcpParameters::default()),
        Err(Error::Collision)
    );
    assert_eq!(ctx.bindings(), Some(&before));
    let extra = offer("a", "recvonly", "");
    let extra = extra.split_once("m=video").unwrap().1;
    let expanded = format!("{actual}m=video{extra}").replace("BUNDLE data v", "BUNDLE data v a");
    let receiver = ctx
        .prepare(
            &expanded,
            &[request("v", true, false), request("a", false, true)],
        )
        .unwrap();
    assert_eq!(
        receiver.bindings().receptions[0].encodings[0].primary,
        Some(101)
    );
}

#[test]
fn bootstrap_slots_correlate_actual_bindings_without_changing_the_applied_answer() {
    let mut generator = context();
    let actual = offer(
        "v",
        "sendonly",
        "a=ssrc:0 cname:native\r\na=ssrc:1 cname:native\r\na=ssrc-group:FID 0 1\r\n",
    );
    let applied = generator
        .prepare(&actual, &[request("v", true, false)])
        .unwrap();
    let mut ctx = Context::new(&actual, applied.answer(), caps(), 900, Limits::default()).unwrap();
    let bindings = crate::adopt_bootstrap_media(
        &mut ctx,
        &[(crate::MediaSlot::LocalVideo("v0".into()), "v".into())],
    )
    .unwrap();
    assert_eq!(
        bindings.publications[0].key,
        MediaKey("local-video:v0".into())
    );
    assert_eq!(bindings.publications[0].encodings[0].primary, Some(0));
    assert_eq!(bindings.publications[0].encodings[0].repair, Some(1));
    assert!(bindings.publications[0].complete());
}

#[test]
fn local_sender_preparation_commits_exact_identities_without_runtime_observation() {
    let mut ctx = Context::new(
        &bootstrap(),
        &remote(),
        caps(),
        u32::MAX - 1,
        Limits::default(),
    )
    .unwrap();
    let actual = offer(
        "v",
        "sendrecv",
        "a=extmap-allow-mixed\r\na=rid:high send\r\na=rid:low send\r\na=simulcast:send high;~low\r\n",
    );
    let unoffered = actual.replace("a=extmap-allow-mixed\r\n", "");
    assert!(!sdp::Document::parse(&unoffered).unwrap().media[1].extmap_allow_mixed);
    let inherited = unoffered.replace("t=0 0\r\n", "t=0 0\r\na=extmap-allow-mixed\r\n");
    assert!(sdp::Document::parse(&inherited).unwrap().media[1].extmap_allow_mixed);
    let mut full = actual.clone();
    let filler = format!("a=x:{}\r\n", "x".repeat(8000));
    while full.len().saturating_add(filler.len()).saturating_add(6) <= MAX_SDP_BYTES {
        full.push_str(&filler);
    }
    full.push_str("a=x:");
    full.extend(std::iter::repeat_n(
        'x',
        MAX_SDP_BYTES.saturating_sub(full.len()).saturating_sub(2),
    ));
    full.push_str("\r\n");
    assert_eq!(full.len(), MAX_SDP_BYTES);
    assert_eq!(ctx.prepare_local_offer(&full), Err(Error::Capacity));
    let prepared = ctx.prepare_local_offer(&actual).unwrap();
    assert!(prepared.contains("a=extmap-allow-mixed\r\n"));
    let sections = inspect(&prepared).unwrap();
    let video = sections.iter().find(|section| section.mid == "v").unwrap();
    assert_eq!(video.encodings.len(), 2);
    assert_eq!(video.encodings[0].primary, Some(u32::MAX));
    assert_eq!(video.encodings[0].repair, Some(1));
    assert_eq!(video.encodings[1].primary, Some(2));
    assert_eq!(video.encodings[1].repair, Some(3));
    assert!(
        video
            .encodings
            .iter()
            .all(|encoding| encoding.rid.is_none())
    );
    assert!(ctx.bindings().is_none());
    let candidate = ctx.prepare(&prepared, &[request("v", true, true)]).unwrap();
    assert!(candidate.answer().contains("a=extmap-allow-mixed\r\n"));
    assert!(candidate.bindings().publications[0].complete());
    let abandoned = candidate.bindings().clone();
    ctx.finish(candidate.id(), ApplyOutcome::NotApplied)
        .unwrap();
    assert!(ctx.bindings().is_none());

    let prepared = ctx.prepare_local_offer(&actual).unwrap();
    let mut candidate = ctx.prepare(&prepared, &[request("v", true, true)]).unwrap();
    let before_modes = candidate.bindings().clone();
    assert_eq!(
        ctx.prepare_sender_modes(&mut candidate, "v", &[Some("bad mode".into()), None]),
        Err(Error::Observation)
    );
    assert_eq!(candidate.bindings(), &before_modes);
    let modes = [Some("L1T1".into()), Some("L1T3".into())];
    ctx.prepare_sender_modes(&mut candidate, "v", &modes)
        .unwrap();
    assert!(ctx.bindings().is_none());
    assert_eq!(
        candidate.bindings().publications[0]
            .encodings
            .iter()
            .map(|encoding| encoding.runtime.scalability_mode.clone())
            .collect::<Vec<_>>(),
        modes
    );
    let next = candidate.bindings().clone();
    let identities = |bindings: &Bindings| {
        bindings
            .publications
            .iter()
            .chain(&bindings.receptions)
            .flat_map(|binding| &binding.encodings)
            .flat_map(|encoding| [encoding.primary, encoding.repair])
            .flatten()
            .collect::<BTreeSet<_>>()
    };
    assert!(identities(&abandoned).is_disjoint(&identities(&next)));
    assert!(next.publications[0].complete());
    assert!(next.receptions[0].complete());
    ctx.finish(candidate.id(), ApplyOutcome::Applied).unwrap();
    assert_eq!(ctx.bindings(), Some(&next));
    let continuing = ctx.prepare_local_offer(&actual).unwrap();
    let candidate = ctx.prepare(&continuing, &[]).unwrap();
    assert_eq!(candidate.bindings(), &next);
    ctx.finish(candidate.id(), ApplyOutcome::Applied).unwrap();
    let expanded = offer(
        "v",
        "sendrecv",
        "a=rid:high send\r\na=rid:medium send\r\na=rid:low send\r\na=simulcast:send high;medium;low\r\n",
    );
    assert_eq!(
        ctx.prepare_local_offer(&expanded),
        Err(Error::Envelope("v".into()))
    );
    assert_eq!(ctx.bindings(), Some(&next));
}

#[test]
fn rid_only_sender_requires_atomic_runtime_completion() {
    let mut ctx = context();
    let actual = offer(
        "v",
        "sendonly",
        "a=rid:high send\r\na=rid:low send\r\na=simulcast:send high;~low\r\n",
    );
    let candidate = ctx.prepare(&actual, &[request("v", true, false)]).unwrap();
    assert!(!candidate.bindings().publications[0].complete());
    assert!(candidate.answer().contains("a=simulcast:recv high;~low"));
    ctx.finish(candidate.id(), ApplyOutcome::Applied).unwrap();
    let mut observations = ctx.bindings().unwrap().publications[0].encodings.clone();
    for (index, encoding) in observations.iter_mut().enumerate() {
        encoding.primary = Some(u32::try_from(index).unwrap() * 2);
        encoding.repair = Some(1 + u32::try_from(index).unwrap() * 2);
    }
    let rtcp = RtcpParameters {
        cname: Some("native".into()),
        reduced_size: false,
    };
    assert_eq!(
        ctx.observe_sender("v", &observations, &RtcpParameters::default()),
        Err(Error::Observation)
    );
    let mut wrong_mode = rtcp.clone();
    wrong_mode.reduced_size = true;
    assert_eq!(
        ctx.observe_sender("v", &observations, &wrong_mode),
        Err(Error::Observation)
    );
    let mut bad = observations.clone();
    bad[1].primary = bad[0].primary;
    assert_eq!(ctx.observe_sender("v", &bad, &rtcp), Err(Error::Collision));
    assert!(!ctx.bindings().unwrap().publications[0].complete());
    let completed = ctx.observe_sender("v", &observations, &rtcp).unwrap();
    assert!(completed.publications[0].complete());
    assert_eq!(completed.publications[0].encodings[0].primary, Some(0));
    assert_eq!(completed.publications[0].rtcp, rtcp);
    assert_eq!(
        ctx.observe_sender("v", &observations, &RtcpParameters::default())
            .unwrap()
            .publications[0]
            .rtcp,
        rtcp
    );
    let mut wrong_cname = rtcp.clone();
    wrong_cname.cname = Some("changed".into());
    assert_eq!(
        ctx.observe_sender("v", &observations, &wrong_cname),
        Err(Error::Observation)
    );
    let retained = ctx.prepare(&actual, &[]).unwrap();
    assert!(retained.bindings().publications[0].complete());
    assert_eq!(retained.bindings().publications[0].rtcp, rtcp);

    let mut cancelled = context();
    let candidate = cancelled
        .prepare(&actual, &[request("v", true, false)])
        .unwrap();
    cancelled
        .finish(candidate.id(), ApplyOutcome::AppliedButCancelled)
        .unwrap();
    assert_eq!(
        cancelled.observe_sender("v", &observations, &rtcp),
        Err(Error::CandidateState)
    );
    cancelled
        .observe_sender_facts("v", &observations, &rtcp)
        .unwrap();
    assert!(cancelled.bindings().is_none());
    assert!(cancelled.applied_bindings().publications[0].complete());
    let stopped = offer("v", "inactive", "").replace("a=bundle-only\r\n", "");
    let candidate = cancelled
        .prepare_retirement(&stopped, &["v".into()])
        .unwrap();
    cancelled
        .finish(candidate.id(), ApplyOutcome::Applied)
        .unwrap();
    let recycled = offer(
        "new",
        "sendonly",
        "a=ssrc:0 cname:native\r\na=ssrc:1 cname:native\r\na=ssrc-group:FID 0 1\r\n",
    );
    assert_eq!(
        cancelled
            .prepare(&recycled, &[request("new", true, false)])
            .unwrap_err(),
        Error::Collision
    );
}

#[test]
fn removed_capabilities_and_transport_changes_do_not_overwrite_accepted_state() {
    let mut ctx = context();
    let actual = offer("v", "recvonly", "");
    let candidate = ctx.prepare(&actual, &[request("v", false, true)]).unwrap();
    ctx.finish(candidate.id(), ApplyOutcome::Applied).unwrap();
    let before = ctx.bindings().unwrap().clone();
    let removed = actual
        .replace("96 97", "98 99")
        .replace(":96", ":98")
        .replace(":97", ":99")
        .replace("apt=96", "apt=98");
    assert!(matches!(
        ctx.prepare(&removed, &[]),
        Err(Error::Envelope(_))
    ));
    assert_eq!(
        ctx.prepare(&actual.replace("ice-ufrag:local", "ice-ufrag:restart"), &[])
            .unwrap_err(),
        Error::TransportChange
    );
    assert_eq!(ctx.bindings(), Some(&before));
    let extra = offer("unused", "inactive", "a=rtcp-rsize\r\n");
    let extra = extra.split_once("m=video").unwrap().1;
    let with_inactive =
        format!("{actual}m=video{extra}").replace("BUNDLE data v", "BUNDLE data v unused");
    let inactive = ctx.prepare(&with_inactive, &[]).unwrap();
    assert!(!inactive.answer().contains("a=rtcp-rsize"));
    assert_eq!(inactive.bindings(), &before);
}

#[test]
fn inactivation_retains_identity_and_stopped_mid_reuse_allocates_fresh_identity() {
    let mut generator = context();
    let actual = offer("v", "recvonly", "");
    let initial = generator
        .prepare(&actual, &[request("v", false, true)])
        .unwrap();
    let answer = initial
        .answer()
        .replace("ssrc:101 ", "ssrc:0 ")
        .replace("FID 101 102", "FID 0 102");
    let mut ctx = Context::new(&actual, &answer, caps(), 100, Limits::default()).unwrap();
    let original = ctx
        .adopt_bootstrap_media(&[request("v", false, true)])
        .unwrap()
        .receptions[0]
        .encodings
        .clone();
    assert_eq!(original[0].primary, Some(0));
    let inactive = ctx
        .prepare(&actual.replace("a=recvonly", "a=inactive"), &[])
        .unwrap();
    assert!(inactive.bindings().receptions.is_empty());
    ctx.finish(inactive.id(), ApplyOutcome::Applied).unwrap();
    let active = ctx.prepare(&actual, &[]).unwrap();
    assert_eq!(active.bindings().receptions[0].encodings, original);
    ctx.finish(active.id(), ApplyOutcome::Applied).unwrap();
    let stopped = ctx
        .prepare(&actual.replace("a=bundle-only\r\n", ""), &[])
        .unwrap();
    assert!(stopped.bindings().receptions.is_empty());
    ctx.finish(stopped.id(), ApplyOutcome::Applied).unwrap();
    assert!(matches!(
        ctx.prepare(&actual, &[]),
        Err(Error::MissingConfiguration(_))
    ));
    let retired_sender = offer(
        "v",
        "sendonly",
        "a=ssrc:0 cname:native\r\na=ssrc:200 cname:native\r\na=ssrc-group:FID 0 200\r\n",
    );
    assert_eq!(
        ctx.prepare(&retired_sender, &[request("v", true, false)])
            .unwrap_err(),
        Error::Collision
    );
    let fresh = ctx.prepare(&actual, &[request("v", false, true)]).unwrap();
    assert_ne!(fresh.bindings().receptions[0].encodings, original);
}

#[test]
fn only_stopped_sections_may_leave_the_current_offer_during_mline_recycling() {
    let mut ctx = context();
    let actual = offer("old", "recvonly", "");
    let first = ctx
        .prepare(&actual, &[request("old", false, true)])
        .unwrap();
    let retired_identity = first.bindings().receptions[0].encodings.clone();
    ctx.finish(first.id(), ApplyOutcome::Applied).unwrap();
    let replacement = offer("fresh", "recvonly", "");
    assert_eq!(
        ctx.prepare(&replacement, &[request("fresh", false, true)])
            .unwrap_err(),
        Error::Envelope("m-line history".into())
    );
    let stopped = ctx
        .prepare(&actual.replace("a=bundle-only\r\n", ""), &[])
        .unwrap();
    ctx.finish(stopped.id(), ApplyOutcome::Applied).unwrap();
    let fresh = ctx
        .prepare(&replacement, &[request("fresh", false, true)])
        .unwrap();
    assert_ne!(fresh.bindings().receptions[0].encodings, retired_identity);
    assert_eq!(
        inspect(fresh.answer())
            .unwrap()
            .iter()
            .map(|section| section.mid.as_str())
            .collect::<Vec<_>>(),
        vec!["data", "fresh"]
    );
    ctx.finish(fresh.id(), ApplyOutcome::Applied).unwrap();
    assert!(
        ctx.prepare(&bootstrap(), &[]).is_err(),
        "a live replacement still cannot disappear"
    );
}

#[test]
fn failed_sender_preparation_can_be_retried_without_reusing_receive_reservations() {
    let mut ctx = context();
    let actual = offer(
        "v",
        "sendrecv",
        "a=ssrc:101 cname:native\r\na=ssrc:102 cname:native\r\na=ssrc-group:FID 101 102\r\n",
    );
    let first = ctx.prepare(&actual, &[request("v", true, true)]).unwrap();
    assert_ne!(
        first.bindings().receptions[0].encodings[0].primary.unwrap(),
        101
    );
    ctx.finish(first.id(), ApplyOutcome::NotApplied).unwrap();
    let retry = ctx.prepare(&actual, &[request("v", true, true)]).unwrap();
    assert_eq!(first.bindings().publications, retry.bindings().publications);
    assert_ne!(first.bindings().receptions, retry.bindings().receptions);
}

#[test]
fn current_offer_orders_bundle_and_sections_and_does_not_reuse_removed_extensions() {
    let mut ctx = context();
    let actual = offer("v", "recvonly", "");
    let first = ctx.prepare(&actual, &[request("v", false, true)]).unwrap();
    ctx.finish(first.id(), ApplyOutcome::Applied).unwrap();
    let (session, media) = actual.split_once("m=application").unwrap();
    let (data, video) = media.split_once("m=video").unwrap();
    let reordered = format!(
        "{}m=video{}m=application{}",
        session.replace("BUNDLE data v", "BUNDLE v data"),
        video.replace(&format!("a=extmap:1 {MID}\r\n"), ""),
        data
    );
    let next = ctx.prepare(&reordered, &[]).unwrap();
    assert!(next.answer().contains("a=group:BUNDLE v data"));
    assert!(!next.answer().contains("a=extmap:"));
    let inspected = inspect(next.answer()).unwrap();
    assert_eq!(inspected[0].mid, "v");
    assert_eq!(inspected[1].mid, "data");
    assert!(next.bindings().receptions[0].extensions.is_empty());
}

#[test]
fn batched_independent_encodings_have_grouping_and_connection_scoped_collision_history() {
    // Vary MID order and allocator seeds, including wrap and zero, without
    // depending on parser/renderer round-trip agreement for the identity oracle.
    for seed in [0, 199, 201, u32::MAX] {
        for reverse in [false, true] {
            let a = offer(
                "a",
                "sendonly",
                "a=ssrc:200 cname:a\r\na=ssrc:201 cname:a\r\na=ssrc-group:FID 200 201\r\n",
            );
            let b = offer("b", "recvonly", "");
            let extra = b.split_once("m=video").unwrap().1;
            let actual = format!("{a}m=video{extra}").replace("BUNDLE data a", "BUNDLE data a b");
            let mut requests = vec![request("a", true, false), request("b", false, true)];
            if reverse {
                requests.reverse();
            }
            let mut ctx =
                Context::new(&bootstrap(), &remote(), caps(), seed, Limits::default()).unwrap();
            let candidate = ctx.prepare(&actual, &requests).unwrap();
            let p = &candidate.bindings().publications[0];
            let r = &candidate.bindings().receptions[0];
            assert_eq!(p.group, r.group);
            if seed == u32::MAX {
                assert_eq!(r.encodings[0].primary, Some(1));
            }
            let identities: BTreeSet<_> = p
                .encodings
                .iter()
                .chain(&r.encodings)
                .flat_map(|e| [e.primary, e.repair])
                .flatten()
                .collect();
            assert_eq!(identities.len(), 4);
            ctx.finish(candidate.id(), ApplyOutcome::Applied).unwrap();
            let before = ctx.bindings().unwrap().clone();
            let invalid = actual
                .replace("ssrc:200", "ssrc:201")
                .replace("FID 200 201", "FID 201 201");
            assert!(ctx.prepare(&invalid, &[]).is_err());
            assert_eq!(ctx.bindings(), Some(&before));
        }
    }
}

#[test]
fn fresh_codec_fallback_keeps_reference_and_negotiates_only_explicit_fmtp() {
    let mut capabilities = caps();
    capabilities.codecs.push(CodecCapability {
        repair: false,
        parameters: CodecParameters {
            name: "h264".into(),
            clock_rate: 90000,
            channels: None,
            fmtp: [
                ("packetization-mode".into(), "1".into()),
                ("profile-level-id".into(), "42e01f".into()),
            ]
            .into_iter()
            .collect(),
            feedback: BTreeSet::new(),
        },
    });
    let mut ctx =
        Context::new(&bootstrap(), &remote(), capabilities, 5, Limits::default()).unwrap();
    let original = offer("v", "recvonly", "");
    let first = ctx
        .prepare(&original, &[request("v", false, true)])
        .unwrap();
    assert!(first.answer().contains("a=rtpmap:96 VP8/90000"));
    ctx.finish(first.id(), ApplyOutcome::Applied).unwrap();
    let h264 = offer(
        "fresh",
        "recvonly",
        "a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\r\n",
    )
    .replace("VP8", "H264");
    let extra = h264.split_once("m=video").unwrap().1;
    let both = format!("{original}m=video{extra}").replace("BUNDLE data v", "BUNDLE data v fresh");
    let mut fresh = request("fresh", false, true);
    fresh.reception.as_mut().unwrap().key = MediaKey("fallback".into());
    let candidate = ctx.prepare(&both, &[fresh]).unwrap();
    assert_eq!(
        candidate.bindings().receptions[0],
        first.bindings().receptions[0]
    );
    let replacement = &candidate.bindings().receptions[1];
    assert_eq!(replacement.codec.parameters.name, "h264");
    assert!(candidate.answer().contains("a=rtpmap:96 H264/90000"));
    assert!(candidate.answer().contains("a=rtpmap:96 VP8/90000"));
    assert_eq!(replacement.codec.parameters.fmtp["packetization-mode"], "1");
    assert_ne!(
        replacement.encodings[0].primary,
        first.bindings().receptions[0].encodings[0].primary
    );
    ctx.finish(candidate.id(), ApplyOutcome::NotApplied)
        .unwrap();
    assert!(matches!(
        ctx.prepare(
            &both.replace("packetization-mode=1", "packetization-mode=0"),
            &[request("fresh", false, true)]
        ),
        Err(Error::Envelope(_))
    ));
}

#[test]
fn descriptions_scale_without_count_ceilings_and_capacity_remains_non_destructive() {
    let mut ctx = Context::new(
        &bootstrap(),
        &remote(),
        caps(),
        u32::MAX,
        Limits {
            max_identity_bytes: identity_history::guard_bytes(2).unwrap(),
        },
    )
    .unwrap();
    let actual = offer("v", "recvonly", "");
    let first = ctx.prepare(&actual, &[request("v", false, true)]).unwrap();
    ctx.finish(first.id(), ApplyOutcome::NotApplied).unwrap();
    assert_eq!(
        ctx.prepare(&actual, &[request("v", false, true)])
            .unwrap_err(),
        Error::Capacity
    );
    assert!(ctx.bindings().is_none());
    assert!(!format!("{first:?}").contains("remote-password"));
    assert!(!format!("{first:?}").contains("a=ice-pwd"));

    let mut actual = bootstrap();
    let mut mids = Vec::new();
    let mut requests = Vec::new();
    let mut slots = Vec::new();
    let mut allocations = Vec::new();
    for index in 0u32..257 {
        let mid = format!("v{index}");
        let primary = 10_000 + 2 * index;
        let repair = primary + 1;
        let section = offer(
            &mid,
            "sendonly",
            &format!(
                "a=ssrc:{primary} cname:native\r\na=ssrc:{repair} cname:native\r\na=ssrc-group:FID {primary} {repair}\r\n"
            ),
        );
        actual.push_str("m=video");
        actual.push_str(section.split_once("m=video").unwrap().1);
        let allocation = Allocation {
            slot: crate::MediaSlot::LocalVideo(format!("camera{index}")),
            format: None,
        };
        requests.push(Preparation {
            mid: mid.clone(),
            payload_type: 96,
            publication: Some(allocation.owner()),
            reception: None,
        });
        slots.push((allocation.slot.clone(), mid.clone()));
        allocations.push(allocation);
        mids.push(mid);
    }
    actual = actual.replace("BUNDLE data", &format!("BUNDLE data {}", mids.join(" ")));
    assert!(actual.len() < MAX_SDP_BYTES);
    let sections = inspect(&actual).unwrap();
    assert_eq!(sections.len(), 258);
    let assigned = assigned_slot_bindings(&actual, slots).unwrap();
    assert_eq!(assigned.len(), 257);
    assert_eq!(assigned.last().unwrap().media_index, 257);
    assert_eq!(assigned.last().unwrap().mid, "v256");

    let excessive = vec![requests[0].clone(); sections.len() + 1];
    let mut ids = crate::id::IdGenerator::new();
    let mut executor = Executor::new(context());
    let invalid = ids.operation();
    executor.enqueue(invalid, excessive.clone()).unwrap();
    assert_eq!(executor.start(), Some(invalid));
    assert_eq!(
        executor.prepare(invalid, &actual).unwrap_err(),
        Error::Capacity
    );
    assert_eq!(
        executor.finish(invalid, false),
        Ok(ApplyOutcome::NotApplied)
    );
    assert!(executor.context_mut().bindings().is_none());
    let operation = ids.operation();
    executor.enqueue_allocation(operation, allocations).unwrap();
    assert_eq!(executor.start(), Some(operation));
    assert_eq!(executor.allocation_targets(operation).unwrap().len(), 257);
    let candidate = executor
        .prepare_allocated(operation, &actual, &assigned)
        .unwrap();
    assert_eq!(candidate.bindings().publications.len(), 257);
    assert!(
        candidate
            .bindings()
            .publications
            .iter()
            .all(Binding::complete)
    );
    let answer = candidate.answer().to_owned();
    let expected = candidate.bindings().clone();
    executor.finish(operation, true).unwrap();
    let continued = ids.operation();
    executor.enqueue(continued, requests.clone()).unwrap();
    assert_eq!(executor.start(), Some(continued));
    let candidate = executor.prepare(continued, &actual).unwrap();
    assert_eq!(candidate.bindings(), &expected);
    executor.finish(continued, true).unwrap();

    let binding = &expected.publications[0];
    let observation = binding.encodings[0].clone();
    for observations in [
        vec![observation.clone(); 2],
        vec![Encoding {
            rid: Some("x".repeat(65)),
            ..observation.clone()
        }],
        vec![Encoding {
            runtime: RuntimeEncoding {
                scalability_mode: Some("x".repeat(65)),
                ..observation.runtime.clone()
            },
            ..observation.clone()
        }],
    ] {
        assert_eq!(
            executor
                .context_mut()
                .observe_sender(&binding.mid, &observations, &binding.rtcp),
            Err(Error::Observation)
        );
        assert_eq!(executor.context_mut().bindings(), Some(&expected));
    }
    let mut bad_rtcp = binding.rtcp.clone();
    bad_rtcp.cname = Some("x".repeat(256));
    assert_eq!(
        executor
            .context_mut()
            .observe_sender(&binding.mid, &[observation], &bad_rtcp),
        Err(Error::Observation)
    );
    assert_eq!(executor.context_mut().bindings(), Some(&expected));

    let mut adopted = Context::new(&actual, &answer, caps(), 100, Limits::default()).unwrap();
    assert_eq!(
        adopted.adopt_bootstrap_media(&excessive),
        Err(Error::Capacity)
    );
    assert!(adopted.bindings().is_none());
    assert_eq!(adopted.adopt_bootstrap_media(&requests).unwrap(), &expected);

    let retirement = ids.operation();
    executor
        .enqueue_retirement(retirement, mids.clone())
        .unwrap();
    assert_eq!(executor.start(), Some(retirement));
    assert_eq!(executor.retirement_targets(retirement).unwrap(), mids);
    assert!(!executor.cancel(retirement));
    assert!(!executor.should_abort(retirement));
    let stopped = actual
        .replace("a=bundle-only\r\n", "")
        .replace("a=sendonly", "a=inactive");
    let candidate = executor.prepare(retirement, &stopped).unwrap();
    assert_eq!(candidate.bindings(), &Bindings::default());
    assert_eq!(
        executor.finish(retirement, true),
        Ok(ApplyOutcome::AppliedButCancelled)
    );
    assert!(executor.context_mut().bindings().is_none());
    assert!(mids.iter().all(|mid| executor.context_mut().stopped(mid)));
    let continued = ids.operation();
    executor.enqueue(continued, vec![]).unwrap();
    assert_eq!(executor.start(), Some(continued));
    let candidate = executor.prepare(continued, &stopped).unwrap();
    assert_eq!(candidate.bindings(), &Bindings::default());
    assert_eq!(executor.finish(continued, true), Ok(ApplyOutcome::Applied));
}

#[test]
fn malformed_supported_input_and_bounds_fail_without_credentials_in_errors() {
    for ssrc in ["-1", "4294967296", "invalid"] {
        let actual = offer("v", "sendonly", &format!("a=ssrc:{ssrc} cname:x\r\n"));
        assert_eq!(inspect(&actual), Err(Error::Malformed("ssrc")));
    }
    let duplicate_fid = offer("v", "sendonly", "a=ssrc-group:FID 0 0\r\n");
    assert_eq!(inspect(&duplicate_fid), Err(Error::Malformed("FID")));
    for (cname, field) in [
        (String::new(), "cname"),
        ("bad value".into(), "cname"),
        ("bad\0value".into(), "line"),
        ("x".repeat(256), "cname"),
    ] {
        assert_eq!(
            inspect(&offer(
                "v",
                "sendonly",
                &format!("a=ssrc:0 cname:{cname}\r\n")
            )),
            Err(Error::Malformed(field))
        );
    }
    let longest = "x".repeat(255);
    assert_eq!(
        inspect(&offer(
            "v",
            "sendonly",
            &format!("a=ssrc:0 cname:{longest}\r\n")
        ))
        .unwrap()[1]
            .rtcp
            .cname,
        Some(longest)
    );
    let mixed = offer("v", "sendonly", "a=ssrc:0 cname:x\r\na=ssrc:1 cname:y\r\n");
    assert!(matches!(inspect(&mixed), Err(Error::Envelope(_))));
    for (attributes, name) in [
        ("a=rtcp-mux\r\n", "rtcp-mux"),
        ("a=rtcp-rsize\r\na=rtcp-rsize\r\n", "rtcp-rsize"),
        ("a=rtcp-rsize:invalid\r\n", "rtcp-rsize"),
        ("a=ssrc:0 cname:x\r\na=ssrc:0 cname:x\r\n", "cname"),
    ] {
        assert_eq!(
            inspect(&offer("v", "sendonly", attributes)),
            Err(Error::Malformed(name))
        );
    }
    let duplicate = offer("data", "recvonly", "");
    assert!(inspect(&duplicate).is_err());
    let invalid = offer("v", "recvonly", "a=extmap:0 secret\r\n");
    assert_eq!(inspect(&invalid), Err(Error::Malformed("extmap")));
    assert_eq!(
        inspect(&"x".repeat(MAX_SDP_BYTES + 1)),
        Err(Error::Capacity)
    );
    let mut invalid_caps = caps();
    invalid_caps
        .extensions
        .insert("bad\r\na=ice-pwd:leak".into());
    assert!(matches!(
        Context::new(&bootstrap(), &remote(), invalid_caps, 1, Limits::default()),
        Err(Error::Identifier)
    ));
}
