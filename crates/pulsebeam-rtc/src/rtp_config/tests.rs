use super::*;
use crate::{Connection, ConnectionConfig, ConnectionEntropy, LocalCandidate, SdpOffer};
use std::time::Instant;

fn codec() -> RtpCodecParameters {
    RtpCodecParameters {
        name: "vp8".into(),
        clock_rate: 90000,
        channels: None,
        fmtp: BTreeMap::new(),
        feedback: ["transport-cc".into(), "nack".into()].into_iter().collect(),
    }
}
fn capabilities() -> RtpCapabilities {
    RtpCapabilities {
        codecs: vec![RtpCodecCapability {
            parameters: codec(),
            repair: true,
        }],
        extensions: [
            MID_EXTENSION,
            TWCC_EXTENSION,
            RID_EXTENSION,
            REPAIRED_RID_EXTENSION,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        feedback: PacketFeedbackKind::TransportWide,
        limits: RtpConfigurationLimits::default(),
    }
}
fn binding(mid: &str, primary: u32) -> RtpBinding {
    RtpBinding {
        mid: mid.into(),
        kind: MediaKind::Video,
        rtcp: RtcpParameters {
            cname: "typed-fixture".into(),
            reduced_size: false,
        },
        codec: RtpCodec {
            payload_type: 96,
            parameters: codec(),
            repair_payload_type: Some(97),
        },
        extensions: [
            MID_EXTENSION,
            TWCC_EXTENSION,
            RID_EXTENSION,
            REPAIRED_RID_EXTENSION,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, uri)| RtpExtensionMapping {
            id: u8::try_from(i.checked_add(1).unwrap()).unwrap(),
            uri: uri.into(),
        })
        .collect(),
        encodings: vec![RtpEncoding {
            primary,
            repair: primary.checked_add(1),
            rid: None,
            active: true,
        }],
    }
}
fn beyond_encoding_count() -> RtpConfiguration {
    let bindings = |prefix: &str, base: u32| {
        (0u32..257)
            .map(|index| {
                binding(
                    &format!("{prefix}{index}"),
                    base.checked_add(index.checked_mul(2).unwrap()).unwrap(),
                )
            })
            .collect()
    };
    RtpConfiguration {
        inbound: bindings("in", 10_000),
        outbound: bindings("out", 20_000),
    }
}
fn connection() -> Connection {
    connection_with_limits(crate::ConnectionLimits::default())
}
fn connection_with_limits(limits: crate::ConnectionLimits) -> Connection {
    let source = include_str!("../../tests/fixtures/chrome-representative.sdp");
    let (header, _) = source.split_once("m=audio").unwrap();
    let (_, application) = source.split_once("m=application").unwrap();
    let offer = format!(
        "{}m=application{}a=candidate:1 1 UDP 2130706431 192.0.2.1 50000 typ host\n",
        header.replace("BUNDLE audio video data", "BUNDLE data"),
        application
    );
    Connection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp("192.0.2.2:41000".parse().unwrap())],
            limits,
            ..ConnectionConfig::default()
        },
        SdpOffer::new(offer),
        Instant::now(),
        std::time::UNIX_EPOCH,
        ConnectionEntropy::new([43; 32]),
    )
    .unwrap()
    .connection
}

#[test]
fn data_only_start_declares_feedback_and_atomic_duplex_encoding_configuration() {
    let mut disabled = connection();
    let mut unfunded = capabilities();
    unfunded.codecs.reserve(256);
    unfunded.limits.max_live_metadata_bytes = storage::capability_bytes(&unfunded).unwrap() - 1;
    assert_eq!(
        disabled.set_rtp_capabilities(unfunded),
        Err(RtpConfigurationError::Capacity)
    );
    assert_eq!(disabled.stats().connection.feedback, None);
    assert!(disabled.rtp_configuration().is_none());
    let mut empty = capabilities();
    empty.codecs.clear();
    empty.extensions.clear();
    disabled.set_rtp_capabilities(empty.clone()).unwrap();
    disabled
        .update_rtp(RtpConfigurationPatch::default())
        .unwrap();
    assert_eq!(
        disabled.rtp_configuration(),
        Some(RtpConfiguration::default())
    );
    assert_eq!(
        disabled.update_rtp(RtpConfigurationPatch {
            inbound: vec![binding("unadvertised", 0)],
            ..Default::default()
        }),
        Err(RtpConfigurationError::Unsupported)
    );
    assert_eq!(
        disabled.rtp_configuration(),
        Some(RtpConfiguration::default())
    );
    assert_eq!(
        disabled.set_rtp_capabilities(capabilities()),
        Err(RtpConfigurationError::CapabilityState)
    );
    disabled.set_rtp_capabilities(empty).unwrap();
    let mut connection = connection();
    let mut many = beyond_encoding_count();
    many.outbound[0] = binding("duplex", 600);
    let mut caps = capabilities();
    caps.codecs.extend((0..128).map(|index| RtpCodecCapability {
        parameters: RtpCodecParameters {
            name: "opus".into(),
            clock_rate: 48000,
            channels: Some(2),
            fmtp: BTreeMap::from([(
                "maxaveragebitrate".into(),
                (6000 + index * 1000).to_string(),
            )]),
            feedback: BTreeSet::from(["transport-cc".into()]),
        },
        repair: false,
    }));
    caps.extensions
        .extend((0..257).map(|index| format!("urn:example:capability:{index}")));
    assert_eq!(caps.codecs.len(), 129);
    assert_eq!(caps.extensions.len(), 261);
    let mut installed_caps = caps.clone();
    let capability_bytes = storage::capability_bytes(&installed_caps).unwrap();
    caps.limits.max_live_metadata_bytes = many
        .inbound
        .iter()
        .map(|binding| (binding, false))
        .chain(many.outbound.iter().map(|binding| (binding, true)))
        .map(|(binding, outbound)| {
            storage::live_bytes(&CurrentBinding {
                binding: binding.clone(),
                outbound,
                order: 0,
            })
            .unwrap()
        })
        .sum::<usize>()
        + capability_bytes;
    installed_caps.limits = caps.limits;
    assert_eq!(connection.stats().connection.feedback, None);
    let configured = RtpConfiguration {
        inbound: vec![binding("duplex", 0)],
        outbound: vec![binding("duplex", 200)],
    };
    assert_eq!(
        connection.configure_rtp(configured.clone()),
        Err(RtpConfigurationError::CapabilityState)
    );
    assert_eq!(
        connection.update_rtp(RtpConfigurationPatch::default()),
        Err(RtpConfigurationError::CapabilityState)
    );
    assert!(connection.rtp_configuration().is_none());
    connection.set_rtp_capabilities(installed_caps).unwrap();
    assert_eq!(
        connection.stats().connection.feedback,
        Some(PacketFeedbackKind::TransportWide)
    );
    connection
        .update_rtp(RtpConfigurationPatch::default())
        .unwrap();
    assert_eq!(
        connection.rtp_configuration(),
        Some(RtpConfiguration::default())
    );
    connection.configure_rtp(configured.clone()).unwrap();
    assert_eq!(connection.rtp_configuration().as_ref(), Some(&configured));
    let mut invalid = configured.clone();
    invalid.inbound.push(binding("extra", 200));
    assert_eq!(
        connection.configure_rtp(invalid),
        Err(RtpConfigurationError::Collision)
    );
    assert_eq!(connection.rtp_configuration().as_ref(), Some(&configured));
    assert_eq!(connection.stats().senders[0].ssrc, 200);
    connection.set_rtp_capabilities(caps.clone()).unwrap();
    let mut changed = caps;
    changed.feedback = PacketFeedbackKind::Rfc8888;
    assert!(connection.set_rtp_capabilities(changed).is_err());
    assert_eq!(connection.rtp_configuration().as_ref(), Some(&configured));

    for patch in [
        RtpConfigurationPatch {
            inbound: vec![binding("extra", 200)],
            remove_encodings: BTreeSet::from([0]),
            ..Default::default()
        },
        RtpConfigurationPatch {
            inbound: configured.inbound.clone(),
            remove_encodings: BTreeSet::from([0]),
            ..Default::default()
        },
        RtpConfigurationPatch {
            outbound: vec![binding("duplicate", 400), binding("duplicate", 600)],
            ..Default::default()
        },
    ] {
        assert!(connection.update_rtp(patch).is_err());
        assert_eq!(connection.rtp_configuration().as_ref(), Some(&configured));
        assert_eq!(connection.stats().senders[0].ssrc, 200);
    }

    let mut grouped = binding("group", 400);
    grouped.encodings[0].rid = Some("q".into());
    grouped.encodings.push(RtpEncoding {
        primary: 402,
        repair: Some(403),
        rid: Some("h".into()),
        active: true,
    });
    connection
        .update_rtp(RtpConfigurationPatch {
            inbound: vec![grouped.clone()],
            ..Default::default()
        })
        .unwrap();
    let mut accepted = configured.clone();
    accepted.inbound.push(grouped);
    assert_eq!(connection.rtp_configuration(), Some(accepted.clone()));
    connection
        .update_rtp(RtpConfigurationPatch {
            remove_encodings: BTreeSet::from([400]),
            ..Default::default()
        })
        .unwrap();
    accepted.inbound[1].encodings.remove(0);
    assert_eq!(connection.rtp_configuration(), Some(accepted.clone()));
    connection
        .update_rtp(RtpConfigurationPatch {
            outbound: vec![binding("duplex", 600)],
            remove_encodings: BTreeSet::from([200, 402]),
            ..Default::default()
        })
        .unwrap();
    accepted.inbound.pop();
    accepted.outbound[0] = binding("duplex", 600);
    assert_eq!(connection.rtp_configuration(), Some(accepted.clone()));
    for _ in 0..2 {
        connection
            .update_rtp(RtpConfigurationPatch {
                remove_encodings: BTreeSet::from([200, 400, 402]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(connection.rtp_configuration(), Some(accepted.clone()));
        assert_eq!(connection.stats().senders[0].ssrc, 600);
    }
    assert_eq!(
        connection.update_rtp(RtpConfigurationPatch {
            outbound: configured.outbound,
            ..Default::default()
        }),
        Err(RtpConfigurationError::Collision)
    );
    assert_eq!(connection.rtp_configuration(), Some(accepted.clone()));
    let (ingress_guards, transport_guards) = connection.test_inbound_guard_bytes();
    assert_eq!(ingress_guards, 4 * std::mem::size_of::<u32>());
    assert_eq!(transport_guards, ingress_guards);
    connection
        .update_rtp(RtpConfigurationPatch {
            remove_encodings: BTreeSet::from([0]),
            ..Default::default()
        })
        .unwrap();
    accepted.inbound.clear();
    assert_eq!(
        connection.test_inbound_guard_bytes(),
        (
            ingress_guards + 2 * std::mem::size_of::<u32>(),
            transport_guards + 2 * std::mem::size_of::<u32>(),
        ),
        "unobserved primary zero and repair retire into both prepaid native fences"
    );
    let (ingress_guards, transport_guards) = connection.test_inbound_guard_bytes();
    assert_eq!(
        ingress_guards + transport_guards,
        6 * crate::ingress::SSRC_GUARD_BYTES
    );
    assert_eq!(connection.rtp_configuration(), Some(accepted));
    assert_eq!(connection.stats().senders[0].ssrc, 600);
    connection.configure_rtp(many.clone()).unwrap();
    assert_eq!(connection.rtp_configuration(), Some(many.clone()));
    assert_eq!(connection.stats().senders.len(), 257);
    assert_eq!(
        connection.update_rtp(RtpConfigurationPatch {
            inbound: vec![binding("new", 30_000)],
            ..Default::default()
        }),
        Err(RtpConfigurationError::Capacity)
    );
    assert_eq!(connection.rtp_configuration(), Some(many.clone()));
    let replacement = binding("in0", 30_000);
    connection
        .update_rtp(RtpConfigurationPatch {
            inbound: vec![replacement.clone()],
            remove_encodings: BTreeSet::from([10_000]),
            ..Default::default()
        })
        .unwrap();
    many.inbound[0] = replacement;
    assert_eq!(connection.rtp_configuration(), Some(many.clone()));
    assert_eq!(connection.stats().senders.len(), 257);
    for primary in [600, 20_512] {
        assert!(
            connection
                .stats()
                .senders
                .iter()
                .any(|sender| sender.ssrc == primary)
        );
    }
}

#[test]
fn identity_storage_is_bounded_and_failed_updates_do_not_consume_it() {
    let mut connection = connection();
    let mut caps = capabilities();
    caps.limits = RtpConfigurationLimits {
        max_identity_guard_bytes: storage::identity_bytes(4).unwrap(),
        ..RtpConfigurationLimits::default()
    };
    connection.set_rtp_capabilities(caps).unwrap();
    let first = RtpConfiguration {
        inbound: vec![],
        outbound: vec![binding("v", 100)],
    };
    connection.configure_rtp(first.clone()).unwrap();
    connection
        .configure_rtp(RtpConfiguration::default())
        .unwrap();
    let second = RtpConfiguration {
        inbound: vec![],
        outbound: vec![binding("v", 200)],
    };
    connection.configure_rtp(second.clone()).unwrap();
    let third = RtpConfiguration {
        inbound: vec![],
        outbound: vec![binding("v", 300)],
    };
    assert_eq!(
        connection.configure_rtp(third.clone()),
        Err(RtpConfigurationError::Capacity)
    );
    assert_eq!(
        connection.configure_rtp(third),
        Err(RtpConfigurationError::Capacity)
    );
    assert_eq!(connection.rtp_configuration().as_ref(), Some(&second));
    assert_eq!(
        connection.configure_rtp(first),
        Err(RtpConfigurationError::Collision)
    );
    assert_eq!(connection.rtp_configuration().as_ref(), Some(&second));
    assert_eq!(connection.stats().senders.len(), 1);
    assert_eq!(connection.stats().senders[0].ssrc, 200);

    for limits in [
        crate::ConnectionLimits {
            max_rtp_replay_state_bytes: 0,
            ..Default::default()
        },
        crate::ConnectionLimits {
            max_receive_runtime_bytes: 0,
            ..Default::default()
        },
        crate::ConnectionLimits {
            max_sent_history_bytes: 0,
            ..Default::default()
        },
    ] {
        let mut constrained = connection_with_limits(limits);
        if limits.max_receive_runtime_bytes == 0 || limits.max_sent_history_bytes == 0 {
            for _ in 0..2 {
                let declaration = capabilities();
                assert_eq!(
                    constrained.set_rtp_capabilities(declaration),
                    Err(RtpConfigurationError::Capacity)
                );
                assert_eq!(constrained.stats().connection.feedback, None);
                assert!(constrained.rtp_configuration().is_none());
                assert_eq!(
                    constrained.configure_rtp(RtpConfiguration::default()),
                    Err(RtpConfigurationError::CapabilityState)
                );
            }
            continue;
        }
        constrained.set_rtp_capabilities(capabilities()).unwrap();
        assert_eq!(
            constrained.configure_rtp(RtpConfiguration {
                inbound: vec![binding("in", 600)],
                outbound: vec![],
            }),
            Err(RtpConfigurationError::Capacity)
        );
        assert!(constrained.rtp_configuration().is_none());
        let out = RtpConfiguration {
            inbound: vec![],
            outbound: vec![binding("out", 600)],
        };
        constrained.configure_rtp(out.clone()).unwrap();
        for _ in 0..2 {
            assert_eq!(
                constrained.update_rtp(RtpConfigurationPatch {
                    inbound: vec![binding("in", 800)],
                    remove_encodings: BTreeSet::from([600]),
                    ..Default::default()
                }),
                Err(RtpConfigurationError::Capacity)
            );
            assert_eq!(constrained.rtp_configuration(), Some(out.clone()));
            assert_eq!(constrained.stats().senders[0].ssrc, 600);
        }
    }

    let mut formats = capabilities();
    let mut vp9 = codec();
    vp9.name = "vp9".into();
    vp9.fmtp.insert("profile-id".into(), "0".into());
    formats.codecs.push(RtpCodecCapability {
        parameters: vp9.clone(),
        repair: true,
    });
    let mut registry = RtpConfigurationState::with_capabilities(formats).unwrap();
    let mut inbound = binding("group", 0);
    inbound.encodings[0].rid = Some("q".into());
    inbound.encodings.push(RtpEncoding {
        primary: 2,
        repair: Some(3),
        rid: Some("h".into()),
        active: true,
    });
    let mut accepted = RtpConfiguration {
        inbound: vec![inbound],
        outbound: vec![binding("out", 10)],
    };
    let update = registry.stage(accepted.clone()).unwrap();
    assert_eq!(update.new_ssrcs.len(), 6);
    assert!(registry.snapshot().is_none() && registry.issued.is_empty());
    registry.commit(update);
    accepted.inbound[0].encodings.reverse();
    accepted.inbound[0].encodings[0].active = false;
    accepted.inbound[0].extensions.reverse();
    accepted.inbound.push(binding("new", 20));
    let abandoned = registry.stage(accepted.clone()).unwrap();
    assert_eq!(
        abandoned.new_ssrcs.keys().copied().collect::<Vec<_>>(),
        [20, 21]
    );
    drop(abandoned);
    assert_eq!(registry.issued.len(), 6);
    assert!(!registry.issued.contains_key(&20));
    let update = registry.stage(accepted.clone()).unwrap();
    registry.commit(update);
    accepted.inbound.reverse();
    let update = registry.stage(accepted.clone()).unwrap();
    assert!(update.new_ssrcs.is_empty());
    registry.commit(update);
    let update = registry.stage(accepted.clone()).unwrap();
    assert!(update.new_ssrcs.is_empty());
    assert_eq!(registry.live.len(), 4);
    let key = registry.by_mid[&(true, "out".into())];
    let mut paused = registry.records[&key].clone();
    paused.binding.encodings[0].active = false;
    let update = registry
        .stage_patch(RtpConfigurationPatch {
            outbound: vec![paused.binding.clone()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(update.changes.len(), 1);
    assert_eq!(update.bindings.len(), 1);
    assert!(update.new_ssrcs.is_empty());
    assert!(update.scope == ConfigurationScope::Scoped && update.inbound_membership.is_empty());
    assert!(update.removed_inbound.is_empty() && update.removed_outbound.is_empty());
    assert_eq!(registry.payload_types(&update, false).as_ref(), &[96, 97]);
    assert!(registry.payload_types(&update, true).is_empty());
    registry.commit(update);
    accepted.outbound[0].encodings[0].active = false;
    assert_eq!(registry.snapshot(), Some(accepted.clone()));
    let inbound_key = registry.by_mid[&(false, "new".into())];
    let mut inbound = registry.records[&inbound_key].clone();
    inbound.binding.encodings[0].active = false;
    let draft = registry
        .stage_patch(RtpConfigurationPatch {
            inbound: vec![inbound.binding],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        draft.inbound_membership,
        BTreeMap::from([(20, false), (21, false)])
    );
    assert!(draft.removed_inbound.is_empty());
    assert_eq!(registry.payload_types(&draft, false).as_ref(), &[96, 97]);
    drop(draft);
    // Untouched owners keep the shared payload and extension assignments.
    let mut different_apt = paused.clone();
    different_apt.binding.codec.payload_type = 98;
    different_apt.binding.encodings[0].primary = 30;
    different_apt.binding.encodings[0].repair = Some(31);
    assert!(matches!(
        registry.stage_changes(
            BTreeMap::from([(key, Some(different_apt))]),
            registry.next_record
        ),
        Err(RtpConfigurationError::Invalid)
    ));
    let mut changed = paused.clone();
    changed.binding.codec.parameters = vp9.clone();
    changed.binding.encodings[0].primary = 30;
    changed.binding.encodings[0].repair = Some(31);
    assert!(matches!(
        registry.stage_changes(BTreeMap::from([(key, Some(changed))]), registry.next_record),
        Err(RtpConfigurationError::Invalid)
    ));
    let mut changed = paused;
    changed.binding.extensions[1].id = 5;
    assert!(matches!(
        registry.stage_changes(BTreeMap::from([(key, Some(changed))]), registry.next_record),
        Err(RtpConfigurationError::Invalid)
    ));
    let mut opposite = binding("out", 30);
    opposite.codec.parameters = vp9.clone();
    opposite.codec.payload_type = 98;
    opposite.codec.repair_payload_type = Some(99);
    let next = registry.next_record.unwrap();
    let opposite = CurrentBinding {
        outbound: false,
        order: next,
        binding: opposite,
    };
    assert!(matches!(
        registry.stage_changes(
            BTreeMap::from([(next, Some(opposite))]),
            next.checked_add(1)
        ),
        Err(RtpConfigurationError::Invalid)
    ));
    assert_eq!(registry.snapshot(), Some(accepted.clone()));
    let update = registry.stage(RtpConfiguration::default()).unwrap();
    registry.commit(update);
    assert!(registry.live.is_empty());
    assert_eq!(registry.snapshot(), Some(RtpConfiguration::default()));
    assert_eq!(registry.issued.len(), 8);
    assert!(
        registry.payloads.is_empty()
            && registry.active_payloads.is_empty()
            && registry.extension_ids.is_empty()
            && registry.extension_uris.is_empty()
    );
    let mut fresh = binding("reassigned", 30);
    fresh.codec.parameters = vp9;
    assert!(
        registry
            .stage(RtpConfiguration {
                inbound: vec![fresh],
                outbound: vec![]
            })
            .is_ok()
    );
    let update = registry.stage(RtpConfiguration::default()).unwrap();
    let (reserved, new) = registry.inbound_reservation(&update);
    assert_eq!(reserved, 0);
    assert!(new.is_empty());
    let fresh = registry
        .stage(RtpConfiguration {
            inbound: vec![binding("fresh", 30)],
            outbound: vec![binding("other", 40)],
        })
        .unwrap();
    let (reserved, new) = registry.inbound_reservation(&fresh);
    assert_eq!(reserved, 2);
    assert_eq!(new, BTreeSet::from([30, 31]));
    assert_eq!(registry.inbound_reserved, 0);
    assert!(matches!(
        registry.stage(accepted.clone()),
        Err(RtpConfigurationError::Collision)
    ));
}

#[test]
fn metadata_storage_and_guard_budgets_replace_binding_and_lifetime_quotas() {
    assert!(storage::identity_bytes(usize::MAX).is_none());
    let caps = capabilities();
    let capability_bytes = storage::capability_bytes(&caps).unwrap();
    let mut registry = RtpConfigurationState::with_capabilities(caps).unwrap();
    let many = beyond_encoding_count();
    let update = registry.stage(many).unwrap();
    registry.commit(update);
    assert_eq!(registry.records.len(), 514);
    assert_eq!(
        registry.live_metadata_bytes,
        registry
            .records
            .values()
            .map(|record| storage::live_bytes(record).unwrap())
            .sum::<usize>()
            + capability_bytes
    );
    let update = registry.stage(RtpConfiguration::default()).unwrap();
    registry.commit(update);
    assert_eq!(registry.live_metadata_bytes, capability_bytes);
    assert_eq!(registry.issued.len(), 1028);

    let mut light = binding("solo", 700);
    light.codec.parameters.name = "vp9".into();
    light
        .codec
        .parameters
        .fmtp
        .insert("profile-id".into(), "0".into());
    let limit = storage::live_bytes(&CurrentBinding {
        outbound: false,
        order: 0,
        binding: light.clone(),
    })
    .unwrap();
    let physical = PhysicalBinding {
        binding: light.clone(),
        outbound: false,
    };
    assert_eq!(physical.ingress_facts().rtcp.as_ref(), Some(&light.rtcp));
    assert_eq!(
        physical
            .egress_facts(crate::id::SenderId::new(1).unwrap())
            .rtcp
            .as_ref(),
        Some(&light.rtcp)
    );
    let mut caps = capabilities();
    caps.codecs.push(RtpCodecCapability {
        parameters: light.codec.parameters.clone(),
        repair: true,
    });
    type InflateCaps = fn(&mut RtpCapabilities);
    let cap_inflaters: [(&str, InflateCaps); 7] = [
        ("codec vector", |c| c.codecs.reserve(100)),
        ("codec name", |c| c.codecs[0].parameters.name.reserve(4096)),
        ("FMTP key", |c| {
            let fmtp = &mut c.codecs[1].parameters.fmtp;
            let (mut key, value) = fmtp.pop_first().unwrap();
            key.reserve(4096);
            fmtp.insert(key, value);
        }),
        ("FMTP value", |c| {
            c.codecs[1]
                .parameters
                .fmtp
                .values_mut()
                .next()
                .unwrap()
                .reserve(4096);
        }),
        ("feedback", |c| {
            let feedback = &mut c.codecs[0].parameters.feedback;
            let mut value = feedback.pop_first().unwrap();
            value.reserve(4096);
            feedback.insert(value);
        }),
        ("extension URI", |c| {
            let mut uri = c.extensions.pop_first().unwrap();
            uri.reserve(4096);
            c.extensions.insert(uri);
        }),
        ("extension entry", |c| {
            c.extensions.insert("urn:example:extra".into());
        }),
    ];
    for (field, inflate) in cap_inflaters {
        let mut heavy = caps.clone();
        heavy.limits.max_live_metadata_bytes = storage::capability_bytes(&heavy).unwrap();
        assert_eq!(heavy.validate(), Ok(heavy.limits.max_live_metadata_bytes));
        inflate(&mut heavy);
        assert_eq!(
            heavy.validate(),
            Err(RtpConfigurationError::Capacity),
            "unaccounted capability {field} storage"
        );
    }
    let mut short_caps = caps.clone();
    short_caps.limits.max_live_metadata_bytes =
        storage::capability_bytes(&short_caps).unwrap() + limit - 1;
    let short = RtpConfigurationState::with_capabilities(short_caps).unwrap();
    assert!(matches!(
        short.stage(RtpConfiguration {
            inbound: vec![light.clone()],
            outbound: vec![],
        }),
        Err(RtpConfigurationError::Capacity)
    ));
    assert!(short.snapshot().is_none() && short.issued.is_empty());
    let capability_bytes = storage::capability_bytes(&caps).unwrap();
    let limit = limit + capability_bytes;
    caps.limits.max_live_metadata_bytes = limit;
    caps.limits.max_identity_guard_bytes = storage::identity_bytes(3).unwrap();
    let mut registry = RtpConfigurationState::with_capabilities(caps).unwrap();
    type Inflate = fn(&mut RtpBinding);
    let inflaters: [(&str, Inflate); 10] = [
        ("MID", |b| b.mid.reserve(4096)),
        ("CNAME", |b| b.rtcp.cname.reserve(4096)),
        ("encodings", |b| b.encodings.reserve(100)),
        ("extensions", |b| b.extensions.reserve(100)),
        ("extension URI", |b| b.extensions[0].uri.reserve(4096)),
        ("RID", |b| {
            let mut rid = String::with_capacity(4096);
            rid.push('q');
            b.encodings[0].rid = Some(rid);
        }),
        ("codec name", |b| b.codec.parameters.name.reserve(4096)),
        ("FMTP key", |b| {
            let (mut key, value) = b.codec.parameters.fmtp.pop_first().unwrap();
            key.reserve(4096);
            b.codec.parameters.fmtp.insert(key, value);
        }),
        ("FMTP value", |b| {
            b.codec
                .parameters
                .fmtp
                .values_mut()
                .next()
                .unwrap()
                .reserve(4096);
        }),
        ("feedback", |b| {
            let mut value = b.codec.parameters.feedback.pop_first().unwrap();
            value.reserve(4096);
            b.codec.parameters.feedback.insert(value);
        }),
    ];
    for (field, inflate) in inflaters {
        let mut heavy = light.clone();
        inflate(&mut heavy);
        assert!(
            matches!(
                registry.stage(RtpConfiguration {
                    inbound: vec![heavy],
                    outbound: vec![]
                }),
                Err(RtpConfigurationError::Capacity)
            ),
            "unaccounted {field} storage"
        );
        assert!(registry.snapshot().is_none() && registry.issued.is_empty());
    }
    let update = registry
        .stage(RtpConfiguration {
            inbound: vec![light.clone()],
            outbound: vec![],
        })
        .unwrap();
    registry.commit(update);
    assert_eq!(registry.live_metadata_bytes, limit);
    let mut paused = light;
    paused.encodings[0].active = false;
    let update = registry
        .stage_patch(RtpConfigurationPatch {
            inbound: vec![paused],
            ..Default::default()
        })
        .unwrap();
    registry.commit(update);
    assert_eq!(registry.live_metadata_bytes, limit);
    let update = registry
        .stage_patch(RtpConfigurationPatch {
            remove_encodings: BTreeSet::from([700]),
            ..Default::default()
        })
        .unwrap();
    registry.commit(update);
    assert_eq!(registry.live_metadata_bytes, capability_bytes);
    assert_eq!(registry.issued.len(), 2);
    assert!(matches!(
        registry.stage(RtpConfiguration {
            inbound: vec![binding("solo", 702)],
            outbound: vec![],
        }),
        Err(RtpConfigurationError::Capacity)
    ));
    let mut bare = binding("solo", 702);
    bare.codec.repair_payload_type = None;
    bare.encodings[0].repair = None;
    let update = registry
        .stage(RtpConfiguration {
            inbound: vec![bare],
            outbound: vec![],
        })
        .unwrap();
    registry.commit(update);
    assert_eq!(registry.issued.len(), 3);
    let before = registry.snapshot();
    let bytes = registry.live_metadata_bytes;
    let mut fresh = binding("solo", 704);
    fresh.codec.repair_payload_type = None;
    fresh.encodings[0].repair = None;
    assert!(matches!(
        registry.stage(RtpConfiguration {
            inbound: vec![fresh],
            outbound: vec![],
        }),
        Err(RtpConfigurationError::Capacity)
    ));
    assert_eq!(registry.snapshot(), before);
    assert_eq!(registry.live_metadata_bytes, bytes);
}

#[test]
fn physical_mutation_and_cross_direction_reuse_are_not_fresh_bindings() {
    let mut connection = connection();
    assert!(!connection.rtp_ssrc_reserved(100));
    assert!(!connection.rtp_ssrc_reserved(101));
    connection.set_rtp_capabilities(capabilities()).unwrap();
    let first = RtpConfiguration {
        inbound: vec![binding("v", 100)],
        outbound: vec![],
    };
    connection.configure_rtp(first.clone()).unwrap();
    assert!(connection.rtp_ssrc_reserved(100));
    assert!(connection.rtp_ssrc_reserved(101));
    let mut extension_order = first.clone();
    extension_order.inbound[0].extensions.reverse();
    connection.configure_rtp(extension_order.clone()).unwrap();
    assert_eq!(
        connection.rtp_configuration().as_ref(),
        Some(&extension_order)
    );
    let mut changed_cname = extension_order.clone();
    changed_cname.inbound[0].rtcp.cname.push('x');
    let mut changed_mode = extension_order.clone();
    changed_mode.inbound[0].rtcp.reduced_size = true;
    for changed in [changed_cname, changed_mode] {
        assert_eq!(
            connection.configure_rtp(changed),
            Err(RtpConfigurationError::Collision)
        );
        assert_eq!(
            connection.rtp_configuration().as_ref(),
            Some(&extension_order)
        );
    }
    connection
        .configure_rtp(RtpConfiguration::default())
        .unwrap();
    assert!(connection.rtp_ssrc_reserved(100));
    assert!(connection.rtp_ssrc_reserved(101));
    let reversed = RtpConfiguration {
        inbound: vec![],
        outbound: first.inbound.clone(),
    };
    assert_eq!(
        connection.configure_rtp(reversed),
        Err(RtpConfigurationError::Collision)
    );
    let mut changed = first.clone();
    changed.inbound[0].mid = "reused".into();
    assert_eq!(
        connection.configure_rtp(changed),
        Err(RtpConfigurationError::Collision)
    );
    assert_eq!(
        connection.configure_rtp(first),
        Err(RtpConfigurationError::Collision)
    );
    assert_eq!(
        connection.rtp_configuration().as_ref(),
        Some(&RtpConfiguration::default())
    );
}

#[test]
fn malformed_and_unsupported_envelopes_cannot_partially_replace_accepted_media() {
    let mut connection = connection();
    connection.set_rtp_capabilities(capabilities()).unwrap();
    let configured = RtpConfiguration {
        inbound: vec![binding("v", 100)],
        outbound: vec![binding("a", 200)],
    };
    connection.configure_rtp(configured.clone()).unwrap();
    let mut boundary = binding("boundary", 400);
    boundary.rtcp.cname = format!("{}a", "é".repeat(127));
    boundary.rtcp.reduced_size = true;
    assert_eq!(boundary.rtcp.cname.len(), usize::from(u8::MAX));
    assert_eq!(validate_binding(&capabilities(), &boundary), Ok(()));
    let mut cases = Vec::new();
    for cname in [
        String::new(),
        "space name".into(),
        "control\0".into(),
        "line\r\n".into(),
        "x".repeat(256),
    ] {
        let mut next = configured.clone();
        next.outbound[0].rtcp.cname = cname;
        assert_eq!(
            validate_binding(&capabilities(), &next.outbound[0]),
            Err(RtpConfigurationError::Invalid)
        );
        cases.push(next);
    }
    let mut next = configured.clone();
    let mut different_apt = binding("other", 400);
    different_apt.codec.payload_type = 98;
    next.outbound.push(different_apt);
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].mid = "bad\r\na=ssrc:1".into();
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].codec.payload_type = 80;
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].codec.repair_payload_type = Some(96);
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].encodings[0].repair = None;
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].extensions[1].id = 1;
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].extensions.remove(1);
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].codec.parameters.name = "unknown".into();
    cases.push(next);
    let mut next = configured.clone();
    next.outbound[0].encodings[0].rid = Some("invalid rid".into());
    cases.push(next);
    for invalid in cases {
        assert!(connection.configure_rtp(invalid).is_err());
        assert_eq!(connection.rtp_configuration().as_ref(), Some(&configured));
        assert_eq!(connection.stats().senders.len(), 1);
    }
}

fn wire(
    primary: u32,
    payload_type: u8,
    sequence: u16,
    mid: &str,
    rid: Option<&str>,
    payload: &[u8],
) -> Vec<u8> {
    let mut bytes = vec![0x90, payload_type];
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes.extend_from_slice(&1234u32.to_be_bytes());
    bytes.extend_from_slice(&primary.to_be_bytes());
    let length = |value: &str| {
        assert!((1..=16).contains(&value.len()));
        u8::try_from(value.len().checked_sub(1).unwrap()).unwrap()
    };
    let mut extensions = vec![0x10 | length(mid)];
    extensions.extend_from_slice(mid.as_bytes());
    if let Some(rid) = rid {
        extensions.push(0x30 | length(rid));
        extensions.extend_from_slice(rid.as_bytes());
    }
    while !extensions.len().is_multiple_of(4) {
        extensions.push(0);
    }
    bytes.extend_from_slice(&[0xbe, 0xde]);
    bytes.extend_from_slice(&u16::try_from(extensions.len() / 4).unwrap().to_be_bytes());
    bytes.extend(extensions);
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn configured_ingress_admits_only_exact_primary_repair_mid_payload_and_rid() {
    for (primary, repair) in [(0, 1), (100, 0)] {
        let now = Instant::now();
        let mut ingress = crate::ingress::IngressOwner::new(
            Box::new([]),
            65536,
            crate::ConnectionLimits::DEFAULT_MAX_RECEIVE_RUNTIME_BYTES,
            9,
            [b'r'; 32],
            crate::receive_feedback::ReceiveFeedback::new(
                Some(PacketFeedbackKind::TransportWide),
                now,
            ),
        )
        .unwrap();
        let mut media = binding("v", primary);
        media.encodings[0].rid = Some("h".into());
        media.encodings[0].repair = Some(repair);
        let physical = PhysicalBinding {
            outbound: false,
            binding: media,
        };
        // Bootstrap narrowing keeps the real receiver, not a second physical lifetime.
        let mut bootstrap_facts = physical.ingress_facts();
        let mut extra = bootstrap_facts.payloads[0].clone();
        extra.payload_type = 98;
        let extra_payload_bytes = std::mem::size_of_val(&extra) + extra.name.len();
        bootstrap_facts.payloads =
            vec![bootstrap_facts.payloads[0].clone(), extra].into_boxed_slice();
        bootstrap_facts.ssrc_repairs = Box::new([]);
        let mut bootstrap = crate::ingress::IngressOwner::new(
            Box::new([bootstrap_facts]),
            65536,
            crate::ConnectionLimits::DEFAULT_MAX_RECEIVE_RUNTIME_BYTES,
            9,
            [b'r'; 32],
            crate::receive_feedback::ReceiveFeedback::new(None, now),
        )
        .unwrap();
        let mut future = physical.clone();
        future.binding.mid = "future".into();
        future.binding.encodings[0].primary = 300;
        future.binding.encodings[0].repair = Some(301);
        let mut first_facts = physical.ingress_facts();
        first_facts.rtcp = None;
        let mut future_facts = future.ingress_facts();
        future_facts.rtcp = None;
        let mut deferred = crate::ingress::IngressOwner::new(
            Box::new([first_facts, future_facts]),
            65536,
            crate::ConnectionLimits::DEFAULT_MAX_RECEIVE_RUNTIME_BYTES,
            9,
            [b'r'; 32],
            crate::receive_feedback::ReceiveFeedback::new(None, now),
        )
        .unwrap();
        let update = deferred
            .stage_configuration(
                &[(physical.clone(), true)],
                &[],
                ConfigurationScope::Bootstrap,
            )
            .unwrap();
        assert!(!deferred.retiring_ssrcs(&update).contains(&300));
        deferred.commit_configuration(update);
        deferred.accept_rtp(
            now,
            wire(300, 96, 1, "future", Some("h"), b"not-admitted"),
            None,
        );
        assert!(deferred.poll_event().is_none());
        let update = deferred
            .stage_configuration(&[(future.clone(), true)], &[], ConfigurationScope::Scoped)
            .unwrap();
        assert!(deferred.retiring_ssrcs(&update).is_empty());
        deferred.commit_configuration(update);
        deferred.accept_rtp(
            now,
            wire(300, 96, 1, "future", Some("h"), b"later-adopted"),
            None,
        );
        assert!(matches!(
            deferred.poll_event(),
            Some(crate::Event::EncodingDiscovered(_))
        ));
        assert!(
            matches!(deferred.poll_event(), Some(crate::Event::Rtp { packet, .. }) if packet.payload.as_ref() == b"later-adopted")
        );
        let owner_index = std::mem::size_of::<(u32, u64)>();
        bootstrap.accept_rtp(
            now,
            wire(primary, 96, 1, "v", Some("h"), b"bootstrap"),
            None,
        );
        assert!(matches!(
            bootstrap.poll_event(),
            Some(crate::Event::EncodingDiscovered(_))
        ));
        assert!(matches!(
            bootstrap.poll_event(),
            Some(crate::Event::Rtp { .. })
        ));
        let mut learned_repair = wire(repair, 97, 50, "v", Some("h"), b"\0\x02bootstrap-repair");
        learned_repair[18] = 0x40;
        bootstrap.accept_rtp(now, learned_repair, None);
        assert!(matches!(
            bootstrap.poll_event(),
            Some(crate::Event::Rtp { .. })
        ));
        bootstrap.accept_rtp(
            now,
            wire(primary, 98, 3, "v", None, b"obsolete-format"),
            None,
        );
        let mut changed = physical.clone();
        changed.binding.encodings[0].repair = Some(299);
        assert!(matches!(
            bootstrap.stage_configuration(&[(changed, true)], &[], true),
            Err(RtpConfigurationError::Collision)
        ));
        for change_cname in [false, true] {
            let mut changed_rtcp = physical.clone();
            if change_cname {
                changed_rtcp.binding.rtcp.cname.push('x');
            } else {
                changed_rtcp.binding.rtcp.reduced_size = !changed_rtcp.binding.rtcp.reduced_size;
            }
            assert!(matches!(
                bootstrap.stage_configuration(
                    &[(changed_rtcp, true)],
                    &[],
                    ConfigurationScope::Bootstrap
                ),
                Err(RtpConfigurationError::Collision)
            ));
        }
        let mut changed_rid = physical.clone();
        changed_rid.binding.encodings[0].rid = Some("q".into());
        assert!(matches!(
            bootstrap.stage_configuration(&[(changed_rid, true)], &[], true),
            Err(RtpConfigurationError::Collision)
        ));
        let mut swapped = physical.clone();
        swapped.binding.encodings[0].primary = repair;
        swapped.binding.encodings[0].repair = Some(primary);
        assert!(matches!(
            bootstrap.stage_configuration(&[(swapped, true)], &[], true),
            Err(RtpConfigurationError::Collision)
        ));
        let exact = bootstrap.test_limit_runtime_to_current_usage();
        let bootstrap_reserved = bootstrap.test_runtime_bytes();
        let bootstrap_storage = bootstrap.test_media_bytes();
        assert_eq!(bootstrap_reserved, exact);
        let mut overflow = physical.clone();
        overflow.binding.mid = "overflow".into();
        overflow.binding.encodings[0].primary = 500;
        overflow.binding.encodings[0].repair = Some(501);
        assert!(matches!(
            bootstrap.stage_configuration(&[(physical.clone(), true), (overflow, true)], &[], true),
            Err(RtpConfigurationError::Capacity)
        ));
        assert_eq!(bootstrap.test_runtime_bytes(), bootstrap_reserved);
        assert_eq!(bootstrap.stats()[0].received_packets, 3);
        let update = bootstrap
            .stage_configuration(&[(physical.clone(), true)], &[], true)
            .unwrap();
        bootstrap.commit_configuration(update);
        assert_eq!(
            bootstrap.test_media_bytes(),
            bootstrap_storage + std::mem::size_of::<(u32, u32)>() + owner_index
                - extra_payload_bytes
        );
        assert_eq!(
            bootstrap.test_runtime_bytes(),
            bootstrap_reserved
                .checked_sub(bootstrap_storage)
                .unwrap()
                .checked_add(bootstrap.test_media_bytes())
                .unwrap()
                .checked_sub(owner_index)
                .unwrap()
        );
        assert_eq!(
            bootstrap.test_runtime_bytes(),
            exact + std::mem::size_of::<(u32, u32)>() - extra_payload_bytes
        );
        assert!(bootstrap.poll_event().is_none());
        assert_eq!(bootstrap.stats().len(), 1);
        assert_eq!(bootstrap.stats()[0].received_packets, 3);
        bootstrap.accept_rtp(now, wire(primary, 96, 4, "v", None, b"continuing"), None);
        assert!(
            matches!(bootstrap.poll_event(), Some(crate::Event::Rtp { packet, .. }) if packet.payload.as_ref() == b"continuing")
        );
        assert!(bootstrap.poll_event().is_none());
        bootstrap.accept_rtp(
            now,
            wire(repair, 97, 51, "v", None, b"\0\x05continuing-repair"),
            None,
        );
        assert!(
            matches!(bootstrap.poll_event(), Some(crate::Event::Rtp { packet, .. }) if packet.header.ssrc == primary && packet.header.sequence == 5 && packet.payload.as_ref() == b"continuing-repair")
        );
        assert_eq!(bootstrap.stats()[0].received_packets, 5);
        ingress.accept_rtp(
            now,
            wire(primary, 96, 1, "v", Some("h"), b"undeclared"),
            None,
        );
        assert!(ingress.poll_event().is_none());
        let mut other = physical.clone();
        other.binding.mid = "other".into();
        other.binding.encodings[0].primary = 200;
        other.binding.encodings[0].repair = Some(201);
        let update = ingress
            .stage_configuration(
                &[(physical.clone(), true), (other.clone(), true)],
                &[],
                true,
            )
            .unwrap();
        ingress.commit_configuration(update);
        let reserved = ingress.test_runtime_bytes();
        assert!(
            ingress.stats().is_empty(),
            "declarations reserve future receivers"
        );
        let replacement_budget = reserved + 2 * crate::ingress::SSRC_GUARD_BYTES;
        let mut constrained = crate::ingress::IngressOwner::new(
            Box::new([]),
            65536,
            replacement_budget,
            9,
            [b'r'; 32],
            crate::receive_feedback::ReceiveFeedback::new(
                Some(PacketFeedbackKind::TransportWide),
                now,
            ),
        )
        .unwrap();
        let update = constrained
            .stage_configuration(
                &[(physical.clone(), true), (other.clone(), true)],
                &[],
                true,
            )
            .unwrap();
        constrained.commit_configuration(update);
        let stored = constrained.test_media_bytes();
        let mut future = physical.clone();
        future.binding.mid = "f".into();
        future.binding.encodings[0].primary = 400;
        future.binding.encodings[0].repair = Some(401);
        assert!(matches!(
            constrained.stage_configuration(&[(future.clone(), true)], &[], false),
            Err(RtpConfigurationError::Capacity)
        ));
        assert_eq!(constrained.test_runtime_bytes(), reserved);
        let mut larger = future.clone();
        larger.binding.mid = "future".into();
        assert!(matches!(
            constrained.stage_configuration(&[(larger, true)], &[primary], false),
            Err(RtpConfigurationError::Capacity)
        ));
        assert_eq!(constrained.test_runtime_bytes(), reserved);
        let update = constrained
            .stage_configuration(&[(physical.clone(), false)], &[], false)
            .unwrap();
        constrained.commit_configuration(update);
        assert_eq!(constrained.test_runtime_bytes(), reserved);
        assert_eq!(constrained.test_media_bytes(), stored);
        assert!(matches!(
            constrained.stage_configuration(&[(future.clone(), true)], &[], false),
            Err(RtpConfigurationError::Capacity)
        ));
        let update = constrained
            .stage_configuration(&[], &[primary], false)
            .unwrap();
        constrained.commit_configuration(update);
        assert!(constrained.test_runtime_bytes() < reserved);
        assert!(constrained.test_media_bytes() < stored);
        let update = constrained
            .stage_configuration(&[(future, true)], &[], false)
            .unwrap();
        constrained.commit_configuration(update);
        assert_eq!(constrained.test_runtime_bytes(), replacement_budget);
        assert_eq!(constrained.test_media_bytes(), stored);
        assert!(constrained.stats().is_empty());

        for invalid in [
            wire(700, 96, 1, "v", Some("h"), b"unknown"),
            wire(primary, 96, 1, "x", Some("h"), b"wrong-mid"),
            wire(primary, 98, 1, "v", Some("h"), b"wrong-pt"),
            wire(primary, 96, 1, "v", Some("l"), b"wrong-rid"),
        ] {
            ingress.accept_rtp(now, invalid, None);
            assert!(ingress.poll_event().is_none());
        }
        ingress.accept_rtp(
            now,
            wire(primary, 96, 1, "v", None, b"primary-source"),
            None,
        );
        assert!(matches!(
            ingress.poll_event(),
            Some(crate::Event::EncodingDiscovered(info)) if info.rid.as_deref() == Some("h")
        ));
        let Some(crate::Event::Rtp { mid, rid, packet }) = ingress.poll_event() else {
            panic!("configured primary")
        };
        assert_eq!(mid.as_ref(), "v");
        assert_eq!(rid.as_deref(), Some("h"));
        assert_eq!(packet.payload.as_ref(), b"primary-source");
        ingress.accept_rtp(
            now,
            wire(primary, 96, 2, "v", Some("l"), b"conflicting-rid"),
            None,
        );
        assert!(ingress.poll_event().is_none());
        // RTX is normalized to the configured primary, not admitted as a new source.
        ingress.accept_rtp(
            now,
            wire(repair, 97, 50, "v", None, b"\0\x02repair-source"),
            None,
        );
        let Some(crate::Event::Rtp { packet, .. }) = ingress.poll_event() else {
            panic!("configured repair")
        };
        assert_eq!(packet.header.ssrc, primary);
        assert_eq!(packet.header.sequence, 2);
        assert_eq!(packet.payload.as_ref(), b"repair-source");
        ingress.accept_rtp(now, wire(200, 96, 1, "other", Some("h"), b"other"), None);
        assert!(
            matches!(ingress.poll_event(), Some(crate::Event::EncodingDiscovered(info)) if info.ssrc == 200)
        );
        assert!(
            matches!(ingress.poll_event(), Some(crate::Event::Rtp { packet, .. }) if packet.header.ssrc == 200)
        );
        assert!(matches!(
            ingress.stage_configuration(&[], &[repair], false),
            Err(RtpConfigurationError::Invalid)
        ));
        assert!(matches!(
            ingress.stage_configuration(&[(physical.clone(), false)], &[primary], false),
            Err(RtpConfigurationError::Invalid)
        ));
        let mut extra = physical.clone();
        extra.binding.mid = "extra".into();
        extra.binding.encodings[0].primary = 400;
        extra.binding.encodings[0].repair = Some(401);
        let staged = ingress
            .stage_configuration(&[(extra, true)], &[], false)
            .unwrap();
        drop(staged);
        assert_eq!(ingress.stats().len(), 2);
        let update = ingress
            .stage_configuration(&[(physical.clone(), false)], &[], false)
            .unwrap();
        ingress.commit_configuration(update);
        ingress.accept_rtp(now, wire(primary, 96, 3, "v", None, b"paused"), None);
        assert!(ingress.poll_event().is_none());
        assert!(ingress.stats()[0].retired);
        ingress.accept_rtp(now, wire(200, 96, 2, "other", None, b"unaffected"), None);
        assert!(
            matches!(ingress.poll_event(), Some(crate::Event::Rtp { packet, .. }) if packet.header.ssrc == 200 && packet.payload.as_ref() == b"unaffected")
        );
        let update = ingress
            .stage_configuration(&[(physical.clone(), true)], &[], false)
            .unwrap();
        ingress.commit_configuration(update);
        ingress.accept_rtp(
            now,
            wire(primary, 96, 3, "v", Some("h"), b"re-admitted"),
            None,
        );
        let Some(crate::Event::Rtp { rid, packet, .. }) = ingress.poll_event() else {
            panic!("re-admitted primary without rediscovery")
        };
        assert_eq!(rid.as_deref(), Some("h"));
        assert_eq!(packet.payload.as_ref(), b"re-admitted");
        assert_eq!(ingress.stats()[0].received_packets, 3);
        ingress.accept_rtp(now, wire(primary, 96, 4, "v", None, b"queued-old"), None);
        let update = ingress.stage_configuration(&[], &[primary], false).unwrap();
        let retired = ingress.commit_configuration(update);
        ingress.test_runtime_bytes();
        assert!(retired.contains(&primary) && retired.contains(&repair));
        assert_eq!(ingress.stats().len(), 1);
        assert_eq!(ingress.stats()[0].ssrc, Some(200));
        assert_eq!(ingress.stats()[0].received_packets, 2);
        assert!(ingress.has_issued_ssrc(primary) && ingress.has_issued_ssrc(repair));
        assert!(
            matches!(ingress.poll_event(), Some(crate::Event::EncodingRetired { ssrc, .. }) if ssrc == primary)
        );
        assert!(ingress.poll_event().is_none());
        assert!(matches!(
            ingress.stage_configuration(&[(physical.clone(), true)], &[], false),
            Err(RtpConfigurationError::Collision)
        ));
        let mut replacement = physical;
        replacement.binding.encodings[0].primary = 300;
        replacement.binding.encodings[0].repair = Some(301);
        let update = ingress
            .stage_configuration(&[(replacement, true)], &[], false)
            .unwrap();
        ingress.commit_configuration(update);
        ingress.accept_rtp(now, wire(primary, 96, 5, "v", None, b"late-old"), None);
        ingress.accept_rtp(
            now,
            wire(repair, 97, 51, "v", None, b"\0\x05late-repair"),
            None,
        );
        assert!(ingress.poll_event().is_none());
        ingress.accept_rtp(now, wire(300, 96, 1, "v", Some("h"), b"replacement"), None);
        assert!(
            matches!(ingress.poll_event(), Some(crate::Event::EncodingDiscovered(info)) if info.ssrc == 300)
        );
        assert!(
            matches!(ingress.poll_event(), Some(crate::Event::Rtp { packet, .. }) if packet.header.ssrc == 300 && packet.payload.as_ref() == b"replacement")
        );
        assert_eq!(ingress.stats().len(), 2);
        assert_eq!(ingress.stats()[0].ssrc, Some(200));
        assert_eq!(ingress.stats()[0].received_packets, 2);
        assert_eq!(ingress.stats()[1].received_packets, 1);
        ingress.retire(300).unwrap();
        assert!(matches!(
            ingress.poll_event(),
            Some(crate::Event::EncodingRetired { ssrc: 300, .. })
        ));
        let update = ingress.stage_configuration(&[], &[300], false).unwrap();
        let retired = ingress.commit_configuration(update);
        assert!(retired.contains(&300) && retired.contains(&301));
        assert_eq!(ingress.stats().len(), 1);
        let mut fresh = other;
        fresh.binding.mid = "fresh".into();
        fresh.binding.encodings[0].primary = 400;
        fresh.binding.encodings[0].repair = Some(401);
        let update = ingress
            .stage_configuration(&[(fresh, true)], &[], false)
            .unwrap();
        ingress.commit_configuration(update);
        let update = ingress.stage_configuration(&[], &[], true).unwrap();
        let retired = ingress.commit_configuration(update);
        assert!(
            [200, 201, 400, 401]
                .iter()
                .all(|ssrc| retired.contains(ssrc))
        );
        assert!(ingress.stats().is_empty());
        assert!(matches!(
            ingress.poll_event(),
            Some(crate::Event::EncodingRetired { ssrc: 200, .. })
        ));
        assert!(ingress.poll_event().is_none());
        if primary == 0 {
            let desired: Vec<_> = beyond_encoding_count()
                .inbound
                .into_iter()
                .map(|binding| {
                    (
                        PhysicalBinding {
                            outbound: false,
                            binding,
                        },
                        true,
                    )
                })
                .collect();
            let before = ingress.test_runtime_bytes();
            let staged = ingress.stage_configuration(&desired, &[], true).unwrap();
            ingress.commit_configuration(staged);
            assert!(ingress.test_runtime_bytes() > before);
            for index in [0usize, 256] {
                let binding = &desired[index].0.binding;
                let ssrc = binding.encodings[0].primary;
                ingress.accept_rtp(
                    now,
                    wire(ssrc, 96, 1, &binding.mid, None, b"later-source"),
                    None,
                );
                assert!(
                    matches!(ingress.poll_event(), Some(crate::Event::EncodingDiscovered(info)) if info.ssrc == ssrc)
                );
                assert!(
                    matches!(ingress.poll_event(), Some(crate::Event::Rtp { packet, .. }) if packet.header.ssrc == ssrc && packet.payload.as_ref() == b"later-source")
                );
            }
            ingress.accept_rtp(now, wire(10_000, 96, 2, "in0", None, b"continuing"), None);
            assert!(
                matches!(ingress.poll_event(), Some(crate::Event::Rtp { packet, .. }) if packet.header.ssrc == 10_000 && packet.payload.as_ref() == b"continuing")
            );
            assert_eq!(ingress.stats().len(), 2);
            assert_eq!(ingress.stats()[0].received_packets, 2);
        }
    }
}

#[test]
#[allow(
    clippy::disallowed_types,
    reason = "exercise the immutable public RTP write payload contract"
)]
fn maximum_typed_namespaces_are_included_before_original_write_admission() {
    let now = Instant::now();
    let mut declared = binding(&"m".repeat(64), 0);
    declared.encodings[0].rid = Some("r".repeat(32));
    validate_binding(&capabilities(), &declared).unwrap();
    let mid = declared.mid.clone();
    let mut egress = crate::egress::MediaEgress::new(
        Box::new([]),
        &[7; 32],
        65536,
        65536,
        crate::ConnectionLimits::DEFAULT_MAX_SEND_RUNTIME_BYTES,
        now,
        crate::time::ProtocolClock::new(now, std::time::UNIX_EPOCH),
    )
    .unwrap();
    let update = egress
        .stage_configuration(
            &[(
                PhysicalBinding {
                    outbound: true,
                    binding: declared,
                },
                true,
            )],
            &[7; 32],
            &[],
            true,
        )
        .unwrap();
    egress.commit_configuration(update);
    let write = |bytes| crate::RtpWrite {
        sequence: 0,
        timestamp: 0,
        wallclock: now,
        marker: false,
        payload: vec![7; bytes].into(),
        extensions: vec![],
    };
    let before = egress.test_runtime_bytes();
    let input = write(usize::from(u16::MAX).checked_sub(96).unwrap());
    let pointer = input.payload.as_ptr();
    let (error, returned) = egress.admit(now, &mid, input).unwrap_err();
    assert_eq!(error, crate::WriteError::InvalidRtp);
    assert_eq!(returned.payload.as_ptr(), pointer);
    assert_eq!(egress.test_runtime_bytes(), before);
    assert_eq!(egress.stats().0.queued_transport_bytes, 0);
    // Base header 12 + extension preamble 4 + MID/RID/TWCC records 104.
    egress
        .admit(
            now,
            &mid,
            write(usize::from(u16::MAX).checked_sub(120).unwrap()),
        )
        .unwrap();
    assert_eq!(egress.stats().1[0].queued_packets, 1);
}

#[test]
#[allow(
    clippy::disallowed_types,
    reason = "exercise the immutable public RTP write payload contract"
)]
fn pause_preserves_egress_but_terminal_omission_requires_a_fresh_identity() {
    let now = Instant::now();
    let mut egress = crate::egress::MediaEgress::new(
        Box::new([]),
        &[7; 32],
        65536,
        65536,
        crate::ConnectionLimits::DEFAULT_MAX_SEND_RUNTIME_BYTES,
        now,
        crate::time::ProtocolClock::new(now, std::time::UNIX_EPOCH),
    )
    .unwrap();
    let physical = PhysicalBinding {
        outbound: true,
        binding: binding("v", 0),
    };
    let id = crate::id::SenderId::new(1).unwrap();
    let (primary, repair, _) = crate::egress::sender_rtp_identity(&[7; 32], id);
    let mut bootstrap_binding = physical.clone();
    bootstrap_binding.binding.encodings[0].primary = primary;
    bootstrap_binding.binding.encodings[0].repair = Some(repair);
    let media_extensions = vec![
        (
            5,
            "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay",
        ),
        (6, crate::negotiation::DEPENDENCY_URIS[0]),
    ];
    bootstrap_binding
        .binding
        .extensions
        .extend(
            media_extensions
                .iter()
                .map(|&(id, uri)| RtpExtensionMapping {
                    id,
                    uri: uri.into(),
                }),
        );
    let expected = crate::negotiation::EgressSenderFacts {
        forwarded_extensions: media_extensions
            .into_iter()
            .map(|(id, uri)| (id, uri.into()))
            .collect(),
        ..bootstrap_binding.egress_facts(id)
    };
    assert_eq!(bootstrap_binding.egress_facts(id), expected);
    let mut bootstrap = crate::egress::MediaEgress::new(
        Box::new([expected]),
        &[7; 32],
        65536,
        65536,
        crate::ConnectionLimits::DEFAULT_MAX_SEND_RUNTIME_BYTES,
        now,
        crate::time::ProtocolClock::new(now, std::time::UNIX_EPOCH),
    )
    .unwrap();
    let adopted = bootstrap
        .stage_configuration(
            &[(bootstrap_binding, true)],
            &[7; 32],
            &[],
            ConfigurationScope::Scoped,
        )
        .unwrap();
    assert_eq!(bootstrap.retiring_ssrcs(&adopted).count(), 0);
    bootstrap.commit_configuration(adopted);
    assert_eq!(bootstrap.stats().1.len(), 1);
    assert_eq!(bootstrap.stats().1[0].ssrc, primary);
    let other = PhysicalBinding {
        outbound: true,
        binding: binding("other", 200),
    };
    let packet = || crate::RtpWrite {
        sequence: 65536,
        timestamp: 90000,
        wallclock: now,
        marker: true,
        payload: bytes::Bytes::from_static(b"opaque-source"),
        extensions: vec![],
    };
    let update = egress
        .stage_configuration(
            &[(physical.clone(), true), (other.clone(), true)],
            &[7; 32],
            &[],
            true,
        )
        .unwrap();
    egress.commit_configuration(update);
    let reserved = egress.test_runtime_bytes();
    let replacement_budget = reserved
        .checked_add(std::mem::size_of::<(u32, u32)>())
        .unwrap();
    let mut constrained = crate::egress::MediaEgress::new(
        Box::new([]),
        &[7; 32],
        65536,
        65536,
        replacement_budget,
        now,
        crate::time::ProtocolClock::new(now, std::time::UNIX_EPOCH),
    )
    .unwrap();
    let update = constrained
        .stage_configuration(
            &[(physical.clone(), true), (other, true)],
            &[7; 32],
            &[],
            true,
        )
        .unwrap();
    constrained.commit_configuration(update);
    let mut fresh = physical.clone();
    fresh.binding.encodings[0].primary = 100;
    fresh.binding.encodings[0].repair = Some(101);
    assert!(matches!(
        constrained.stage_configuration(&[(fresh.clone(), true)], &[7; 32], &[], false),
        Err(RtpConfigurationError::Capacity)
    ));
    assert_eq!(constrained.test_runtime_bytes(), reserved);
    let swap = constrained
        .stage_configuration(&[(fresh.clone(), true)], &[7; 32], &[0], false)
        .unwrap();
    assert_eq!(constrained.test_runtime_bytes(), reserved);
    drop(swap);
    let update = constrained
        .stage_configuration(&[(physical.clone(), false)], &[7; 32], &[], false)
        .unwrap();
    constrained.commit_configuration(update);
    assert_eq!(constrained.test_runtime_bytes(), reserved);
    let update = constrained
        .stage_configuration(&[], &[7; 32], &[0], false)
        .unwrap();
    constrained.commit_configuration(update);
    assert!(matches!(
        constrained.stage_configuration(&[(physical.clone(), true)], &[7; 32], &[], false),
        Err(RtpConfigurationError::Collision)
    ));
    let mut larger = fresh.clone();
    larger.binding.mid = "longermid".into();
    assert!(matches!(
        constrained.stage_configuration(&[(larger, true)], &[7; 32], &[], false),
        Err(RtpConfigurationError::Capacity)
    ));
    let update = constrained
        .stage_configuration(&[(fresh, true)], &[7; 32], &[], false)
        .unwrap();
    constrained.commit_configuration(update);
    assert_eq!(constrained.test_runtime_bytes(), replacement_budget);
    let replacement = PhysicalBinding {
        outbound: true,
        binding: binding("v", 300),
    };
    let update = constrained
        .stage_configuration(&[(replacement.clone(), true)], &[7; 32], &[], true)
        .unwrap();
    constrained.commit_configuration(update);
    constrained.admit(now, "v", packet()).unwrap();
    let update = constrained
        .stage_configuration(&[(replacement, false)], &[7; 32], &[], false)
        .unwrap();
    constrained.commit_configuration(update);
    assert!(constrained.test_runtime_bytes() <= replacement_budget);

    egress.admit(now, "v", packet()).unwrap();
    egress.admit(now, "other", packet()).unwrap();
    let charge = egress.stats().0.queued_transport_bytes;
    assert_eq!(egress.stats().1[0].queued_packets, 1);
    let update = egress
        .stage_configuration(&[(physical.clone(), false)], &[7; 32], &[], false)
        .unwrap();
    assert_eq!(egress.retiring_ssrcs(&update).count(), 0);
    egress.commit_configuration(update);
    assert_eq!(egress.stats().1[0].queued_packets, 0);
    assert_eq!(egress.stats().1[1].ssrc, 200);
    assert_eq!(egress.stats().1[1].queued_packets, 1);
    assert_eq!(egress.stats().0.queued_transport_bytes, charge);
    assert_eq!(
        egress.admit(now, "v", packet()).unwrap_err().0,
        crate::WriteError::UnknownStream
    );
    assert!(matches!(
        egress.stage_configuration(&[], &[7; 32], &[1], false),
        Err(RtpConfigurationError::Invalid)
    ));
    assert!(matches!(
        egress.stage_configuration(&[(physical.clone(), true)], &[7; 32], &[0], false),
        Err(RtpConfigurationError::Invalid)
    ));
    let mut replacement = PhysicalBinding {
        outbound: true,
        binding: binding("v", 100),
    };
    replacement.binding.codec.repair_payload_type = None;
    replacement.binding.encodings[0].repair = None;
    let staged = egress
        .stage_configuration(&[(replacement.clone(), true)], &[7; 32], &[], false)
        .unwrap();
    drop(staged);
    assert_eq!(egress.stats().1.len(), 2);
    let update = egress
        .stage_configuration(&[(physical.clone(), true)], &[7; 32], &[], false)
        .unwrap();
    egress.commit_configuration(update);
    assert_eq!(egress.stats().1.len(), 2);
    assert_eq!(egress.stats().1[0].ssrc, 0);
    let mut next = packet();
    next.sequence = next.sequence.saturating_add(1);
    egress.admit(now, "v", next).unwrap();
    assert_eq!(egress.stats().1[0].queued_packets, 1);
    let update = egress
        .stage_configuration(&[], &[7; 32], &[0], false)
        .unwrap();
    assert_eq!(
        egress.retiring_ssrcs(&update).collect::<Vec<_>>(),
        vec![0, 1]
    );
    egress.commit_configuration(update);
    assert_eq!(egress.stats().1.len(), 1);
    assert_eq!(egress.stats().1[0].ssrc, 200);
    assert_eq!(egress.stats().1[0].queued_packets, 1);
    assert_eq!(
        egress.admit(now, "v", packet()).unwrap_err().0,
        crate::WriteError::UnknownStream
    );
    assert!(matches!(
        egress.stage_configuration(&[(physical, true)], &[7; 32], &[], false),
        Err(RtpConfigurationError::Collision)
    ));
    let update = egress
        .stage_configuration(&[(replacement, true)], &[7; 32], &[], false)
        .unwrap();
    egress.commit_configuration(update);
    assert_eq!(egress.stats().1.len(), 2);
    assert_eq!(egress.stats().1[1].ssrc, 100);
    assert_eq!(egress.stats().1[1].transmitted_packets, 0);
    egress.admit(now, "v", packet()).unwrap();
    assert_eq!(egress.stats().1[1].queued_packets, 1);
    assert_eq!(egress.stats().1[0].queued_packets, 1);
    let update = egress
        .stage_configuration(&[], &[7; 32], &[], true)
        .unwrap();
    assert_eq!(
        egress
            .retiring_ssrcs(&update)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([100, 200, 201])
    );
    egress.commit_configuration(update);
    assert!(egress.stats().1.is_empty());
    let desired: Vec<_> = beyond_encoding_count()
        .outbound
        .into_iter()
        .map(|binding| {
            (
                PhysicalBinding {
                    outbound: true,
                    binding,
                },
                true,
            )
        })
        .collect();
    let before = egress.test_runtime_bytes();
    let staged = egress
        .stage_configuration(&desired, &[7; 32], &[], true)
        .unwrap();
    egress.commit_configuration(staged);
    assert!(egress.test_runtime_bytes() > before);
    for index in [0usize, 256] {
        egress
            .admit(now, &desired[index].0.binding.mid, packet())
            .unwrap();
    }
    let mut next = packet();
    next.sequence = next.sequence.checked_add(1).unwrap();
    egress.admit(now, "out0", next).unwrap();
    assert_eq!(egress.stats().1.len(), 257);
    assert_eq!(egress.stats().1[0].queued_packets, 2);
    assert_eq!(egress.stats().1[256].ssrc, 20_512);
    assert_eq!(egress.stats().1[256].queued_packets, 1);
}

#[test]
fn codec_parameter_model_accepts_opus_vp8_h264_and_representative_vp9_without_enum_conversion() {
    let mut caps = capabilities();
    let mut opus = codec();
    opus.name = "opus".into();
    opus.clock_rate = 48000;
    opus.channels = Some(2);
    opus.fmtp.insert("useinbandfec".into(), "1".into());
    let mut h264 = codec();
    h264.name = "h264".into();
    h264.fmtp.insert("packetization-mode".into(), "1".into());
    h264.fmtp.insert("profile-level-id".into(), "42e01f".into());
    let mut vp9 = codec();
    vp9.name = "vp9".into();
    vp9.fmtp.insert("profile-id".into(), "0".into());
    caps.codecs.extend(
        [opus, h264, vp9]
            .into_iter()
            .map(|parameters| RtpCodecCapability {
                repair: parameters.name != "vp9",
                parameters,
            }),
    );
    caps.validate().unwrap();
    for supported in &caps.codecs {
        let parameters = &supported.parameters;
        let mut media = binding("v", 100);
        media.kind = if parameters.name == "opus" {
            MediaKind::Audio
        } else {
            MediaKind::Video
        };
        media.codec.parameters = parameters.clone();
        let state = RtpConfigurationState::with_capabilities(caps.clone()).unwrap();
        if !supported.repair {
            assert!(matches!(
                state.stage(RtpConfiguration {
                    inbound: vec![media.clone()],
                    outbound: vec![]
                }),
                Err(RtpConfigurationError::Unsupported)
            ));
            media.codec.repair_payload_type = None;
            media.encodings[0].repair = None;
        }
        let config = RtpConfiguration {
            inbound: vec![media.clone()],
            outbound: vec![],
        };
        let update = state.stage(config).unwrap();
        let staged = &update.bindings;
        assert_eq!(staged[0].0.binding.codec.parameters, *parameters);
        assert_eq!(
            staged[0].0.ingress_facts().payloads[0],
            crate::RtpPayloadInfo {
                payload_type: 96,
                name: parameters.name.to_ascii_lowercase().into_boxed_str(),
                clock_rate: parameters.clock_rate,
            }
        );
    }
}
