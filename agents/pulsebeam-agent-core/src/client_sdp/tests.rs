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
        repair: true,
        codecs: vec![CodecParameters {
            name: "vp8".into(),
            clock_rate: 90000,
            channels: None,
            fmtp: BTreeMap::new(),
            feedback: ["nack".into(), "transport-cc".into()].into_iter().collect(),
        }],
        extensions: [MID.to_owned()].into_iter().collect(),
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
fn data_only_bootstrap_prepares_duplex_with_independent_identities_and_rtp_facts() {
    let mut ctx = context();
    let actual = offer(
        "v",
        "sendrecv",
        "a=ssrc:200 cname:native\r\na=ssrc:201 cname:native\r\na=ssrc-group:FID 200 201\r\n",
    );
    let candidate = ctx.prepare(&actual, &[request("v", true, true)]).unwrap();
    let p = &candidate.bindings().publications[0];
    let r = &candidate.bindings().receptions[0];
    assert!(p.complete() && r.complete());
    assert_eq!(p.encodings[0].primary.unwrap().get(), 200);
    assert_ne!(p.encodings[0].primary, r.encodings[0].primary);
    let answer = inspect(candidate.answer()).unwrap();
    assert_eq!(answer[1].codecs[0], r.codec);
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
        "a=ssrc:200 cname:native\r\na=ssrc:201 cname:native\r\na=ssrc-group:FID 200 201\r\n",
    );
    let applied = generator
        .prepare(&actual, &[request("v", true, true)])
        .unwrap();
    let mut ctx = Context::new(&actual, applied.answer(), caps(), 900, Limits::default()).unwrap();
    assert!(matches!(
        ctx.adopt_bootstrap_media(&[]),
        Err(Error::MissingConfiguration(_))
    ));
    assert!(ctx.bindings().is_none());
    assert_eq!(
        ctx.adopt_bootstrap_media(&[request("v", true, true)])
            .unwrap(),
        applied.bindings()
    );
    assert_eq!(
        ctx.adopt_bootstrap_media(&[request("v", true, true)]),
        Err(Error::CandidateState)
    );
    let retained = ctx.prepare(&actual, &[]).unwrap();
    assert_eq!(retained.bindings(), applied.bindings());
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
        encoding.primary = NonZeroU32::new(300 + u32::try_from(index).unwrap() * 2);
        encoding.repair = NonZeroU32::new(301 + u32::try_from(index).unwrap() * 2);
    }
    let mut bad = observations.clone();
    bad[1].primary = bad[0].primary;
    assert_eq!(ctx.observe_sender("v", &bad), Err(Error::Collision));
    assert!(!ctx.bindings().unwrap().publications[0].complete());
    assert!(ctx.observe_sender("v", &observations).unwrap().publications[0].complete());
    let retained = ctx.prepare(&actual, &[]).unwrap();
    assert!(retained.bindings().publications[0].complete());
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
}

#[test]
fn inactivation_retains_identity_and_stopped_mid_reuse_allocates_fresh_identity() {
    let mut ctx = context();
    let actual = offer("v", "recvonly", "");
    let first = ctx.prepare(&actual, &[request("v", false, true)]).unwrap();
    let original = first.bindings().receptions[0].encodings.clone();
    ctx.finish(first.id(), ApplyOutcome::Applied).unwrap();
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
        first.bindings().receptions[0].encodings[0]
            .primary
            .unwrap()
            .get(),
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
    capabilities.codecs.push(CodecParameters {
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
    });
    let mut ctx =
        Context::new(&bootstrap(), &remote(), capabilities, 5, Limits::default()).unwrap();
    let original = offer("v", "recvonly", "");
    let first = ctx
        .prepare(&original, &[request("v", false, true)])
        .unwrap();
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
fn capacity_is_non_destructive_and_error_messages_never_contain_sdp() {
    let mut ctx = Context::new(
        &bootstrap(),
        &remote(),
        caps(),
        0,
        Limits {
            max_identity_history: 2,
            ..Limits::default()
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
}

#[test]
fn malformed_supported_input_and_bounds_fail_without_credentials_in_errors() {
    let actual = offer("v", "sendonly", "a=ssrc:0 cname:x\r\n");
    assert_eq!(inspect(&actual), Err(Error::Malformed("ssrc")));
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
