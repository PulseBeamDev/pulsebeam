use super::*;
use pulsebeam_agent_core::client_sdp as client;
use pulsebeam_rtc_simulation::{
    EncodedH264Source, EncodedVideoAccessUnit, EncodedVideoCodec, EncodedVideoInput,
    EncodedVideoMetadata, MediaType, RtcStatsClass, RtcStatsValues, RtpHeaderExtensionDirection,
    RtpTransceiver, RtpTransceiverDirection, VideoCodecFormat, VideoFrame, VideoSource,
};

const ROOM: &str = "native-state-av1";
const MID: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";
const DD: &str = pulsebeam_core::dd::URI;

enum Source {
    Encoded(EncodedH264Source),
    Stock(VideoSource),
}

async fn connect(
    host: NativeHost,
    server: IpAddr,
    publish: bool,
    dd: bool,
) -> anyhow::Result<(WirePeer, RtpTransceiver, Source, wire::RtpParameters)> {
    let input = (!dd)
        .then(|| EncodedVideoInput::new_for_format(VideoCodecFormat::new("AV1")))
        .transpose()?;
    let bootstrap = input
        .as_ref()
        .map_or(Av1Bootstrap::Stock, |input| Av1Bootstrap::External {
            input,
            remap_payload_types: !publish,
        });
    let mut created = None;
    let direction = if publish {
        RtpTransceiverDirection::SendOnly
    } else {
        RtpTransceiverDirection::ReceiveOnly
    };
    let (mut wire, _) = WirePeer::connect_with_peer(
        host,
        server,
        if publish { "av1-source" } else { "av1-viewer" },
        ROOM,
        true,
        Some(bootstrap),
        |factory, peer| {
            let (source, track) = if let Some(input) = &input {
                let source = input.create_source(factory)?;
                let track = source.create_track(factory, "av1")?;
                (Source::Encoded(source), track)
            } else {
                let source = factory.create_video_source()?;
                let track = factory.create_video_track("av1", &source)?;
                (Source::Stock(source), track)
            };
            let transceiver = peer.add_video_transceiver(&track, direction)?;
            if dd {
                let capabilities = factory
                    .get_rtp_sender_capabilities(MediaType::Video)
                    .codecs();
                let codec = |name: &[u8]| {
                    capabilities
                        .iter()
                        .find(|codec| codec.base_codec().name_bytes() == name)
                        .cloned()
                        .with_context(|| {
                            format!("stock capability {}", String::from_utf8_lossy(name))
                        })
                };
                transceiver.set_rtp_codec_preferences(&[codec(b"AV1")?, codec(b"rtx")?])?;
            }
            let mut extensions = transceiver.header_extensions_to_negotiate()?;
            for extension in &mut extensions {
                match extension.uri() {
                    MID => {}
                    DD if dd => {
                        extension.direction = if publish {
                            RtpHeaderExtensionDirection::SendOnly
                        } else {
                            RtpHeaderExtensionDirection::ReceiveOnly
                        };
                    }
                    _ => extension.direction = RtpHeaderExtensionDirection::Stopped,
                }
            }
            transceiver.set_header_extensions_to_negotiate(&extensions)?;
            created = Some((transceiver, source));
            Ok(())
        },
    )
    .await?;
    let (transceiver, source) = created.context("stock AV1 transceiver")?;
    let descriptions = wire.peer.descriptions()?;
    let answer = descriptions.current_remote.context("applied AV1 answer")?;
    let section = client::inspect(&answer.sdp)?
        .into_iter()
        .find(|section| section.kind == client::Kind::Video)
        .context("negotiated AV1 section")?;
    let codec = section.codecs.first().context("negotiated AV1 codec")?;
    anyhow::ensure!(
        codec.parameters.name == "av1"
            && codec.parameters.feedback.contains("ack ccfb")
            && !codec.parameters.feedback.contains("transport-cc"),
        "unexpected AV1 contract: {codec:?}"
    );
    let owner = client::MediaOwner {
        key: client::MediaKey("camera".into()),
        group: None,
    };
    let adopted = wire
        .media
        .as_mut()
        .context("Native media context")?
        .adopt_bootstrap_media(&[client::Preparation {
            mid: section.mid,
            payload_type: codec.payload_type,
            publication: publish.then(|| owner.clone()),
            reception: (!publish).then_some(owner),
        }])?;
    let binding = if publish {
        adopted.publications.first()
    } else {
        adopted.receptions.first()
    }
    .context("adopted stock AV1")?;
    let [encoding] = binding.encodings.as_slice() else {
        anyhow::bail!("expected one AV1 encoding")
    };
    anyhow::ensure!(
        binding.complete() && encoding.repair.is_some() && encoding.rid.is_none(),
        "incomplete AV1 FID: {binding:?}"
    );
    anyhow::ensure!(
        binding
            .extensions
            .iter()
            .all(|ext| ext.uri == MID || dd && ext.uri == DD)
            && binding.extensions.iter().any(|ext| ext.uri == DD) == dd,
        "AV1 fixture negotiated unexpected extensions: {:?}; DD offered={}, answered={}",
        binding.extensions,
        descriptions
            .current_local
            .as_ref()
            .is_some_and(|local| local
                .sdp
                .lines()
                .any(|line| line.starts_with("a=extmap:") && line.contains(DD))),
        answer
            .sdp
            .lines()
            .any(|line| line.starts_with("a=extmap:") && line.contains(DD))
    );
    let parameters = wire::RtpParameters {
        mid: binding.mid.clone(),
        extmap_allow_mixed: Some(binding.extmap_allow_mixed),
        codec: Some(wire::RtpCodecParameters {
            payload_type: Some(u32::from(binding.codec.payload_type)),
            rtx_payload_type: binding
                .codec
                .repair
                .as_ref()
                .map(|repair| u32::from(repair.payload_type)),
            feedback: binding.codec.parameters.feedback.iter().cloned().collect(),
            format: Some(wire::CodecFormat {
                mime_type: "video/av1".into(),
                clock_rate: binding.codec.parameters.clock_rate,
                channels: None,
                parameters: binding
                    .codec
                    .parameters
                    .fmtp
                    .iter()
                    .map(|(name, value)| wire::CodecParameter {
                        name: name.clone(),
                        value: value.clone(),
                    })
                    .collect(),
            }),
        }),
        header_extensions: binding
            .extensions
            .iter()
            .map(|extension| wire::RtpHeaderExtensionParameters {
                id: u32::from(extension.id),
                uri: extension.uri.clone(),
            })
            .collect(),
        encodings: vec![wire::RtpEncodingParameters {
            ssrc: encoding.primary,
            rtx_ssrc: encoding.repair,
            rid: String::new(),
            scalability_mode: None,
        }],
        rtcp: Some(wire::RtcpParameters {
            cname: binding.rtcp.cname.clone().context("AV1 CNAME")?,
            reduced_size: binding.rtcp.reduced_size,
        }),
    };
    Ok((wire, transceiver, source, parameters))
}

pub(super) fn remap_offer_payload_types(offer: &mut String) -> anyhow::Result<()> {
    let sections = client::inspect(offer)?;
    let section = sections
        .iter()
        .find(|section| section.kind == client::Kind::Video)
        .context("AV1 offer section")?;
    let codec = section
        .codecs
        .iter()
        .find(|codec| codec.parameters.name == "av1")
        .context("AV1 offer codec")?;
    let repair = codec.repair.as_ref().context("AV1 offer RTX")?;
    let mut available = (96_u8..=127).rev().filter(|pt| {
        !sections
            .iter()
            .flat_map(|section| &section.codecs)
            .any(|codec| {
                codec.payload_type == *pt
                    || codec
                        .repair
                        .as_ref()
                        .is_some_and(|repair| repair.payload_type == *pt)
            })
    });
    let primary = available.next().context("unused AV1 payload type")?;
    let rtx = available.next().context("unused AV1 RTX payload type")?;
    let remap = |pt: &str| match pt.parse::<u8>() {
        Ok(pt) if pt == codec.payload_type => primary.to_string(),
        Ok(pt) if pt == repair.payload_type => rtx.to_string(),
        _ => pt.to_owned(),
    };
    let mut output = Vec::new();
    let mut video = false;
    for line in offer.lines() {
        if line.starts_with("m=") {
            video = line.starts_with("m=video ");
            if video {
                let mut fields = line.split_ascii_whitespace();
                let mut media = fields
                    .by_ref()
                    .take(3)
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                media.extend(fields.map(remap));
                output.push(media.join(" "));
                continue;
            }
        }
        if video
            && let Some((attribute, value)) = line.split_once(':')
            && matches!(attribute, "a=rtpmap" | "a=fmtp" | "a=rtcp-fb")
            && let Some((pt, details)) = value.split_once(' ')
        {
            let details = if attribute == "a=fmtp" && pt.parse::<u8>() == Ok(repair.payload_type) {
                let apt = details.strip_prefix("apt=").context("AV1 offer RTX apt")?;
                anyhow::ensure!(apt.parse::<u8>() == Ok(codec.payload_type));
                format!("apt={primary}")
            } else {
                details.to_owned()
            };
            output.push(format!("{attribute}:{} {details}", remap(pt)));
        } else {
            output.push(line.to_owned());
        }
    }
    *offer = output.join("\r\n") + "\r\n";
    Ok(())
}

async fn install(
    peer: &mut WirePeer,
    declaration: wire::resource_spec::Declaration,
) -> anyhow::Result<wire::Track> {
    let receive = matches!(
        &declaration,
        wire::resource_spec::Declaration::Subscription(_)
    )
    .then(|| wire::resource_settings::Receive::Video(Default::default()));
    let spec = wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive,
        }),
        declaration: Some(declaration),
    };
    validation::validate_spec(&spec)
        .map_err(|error| anyhow::anyhow!("AV1 binding spec: {error:?}"))?;
    peer.desire(1, 1, Some(spec.clone()));
    loop {
        let state = peer.confirmed(1, 1).await?;
        anyhow::ensure!(state.rejection.is_none(), "AV1 binding rejected: {state:?}");
        let Some(wire::confirmed_state::Value::Owned(live)) = state.value else {
            anyhow::bail!("AV1 binding absent: {state:?}");
        };
        anyhow::ensure!(live.spec.as_ref() == Some(&spec), "AV1 binding changed");
        anyhow::ensure!(
            live.execution != wire::ExecutionStatus::Failed as i32,
            "AV1 installation failed: {live:?}"
        );
        if live.execution == wire::ExecutionStatus::Installed as i32 {
            return live.track.context("installed AV1 track");
        }
    }
}

async fn publisher_pli_count(peer: &PeerConnection, ssrc: u32) -> anyhow::Result<u32> {
    let request = peer.request_stats()?;
    tokio::time::timeout(Duration::from_millis(250), async {
        loop {
            while let Some(event) = peer.try_next_event() {
                if let PeerConnectionEvent::Stats(snapshot) = event
                    && snapshot.operation_id == request
                {
                    return snapshot
                        .report
                        .of_class(RtcStatsClass::OutboundRtpStreamStats)
                        .into_iter()
                        .find_map(|entry| match entry.values() {
                            RtcStatsValues::OutboundRtpStreamStats(value)
                                if value.ssrc == Some(ssrc) =>
                            {
                                Some(value.pli_count.unwrap_or(0))
                            }
                            _ => None,
                        })
                        .context("AV1 publisher RTP stats");
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .context("AV1 publisher stats request timed out")?
}

async fn session(
    host: NativeHost,
    server: IpAddr,
    cross_shard: bool,
    dd: bool,
) -> anyhow::Result<()> {
    let (mut publisher, _, source, parameters) = connect(host.clone(), server, true, dd).await?;
    let publisher_ssrc = parameters.encodings[0].ssrc.context("AV1 publisher SSRC")?;
    let publisher_codec = parameters.codec.clone().context("AV1 publisher codec")?;
    let track = install(
        &mut publisher,
        wire::resource_spec::Declaration::Publication(wire::PublicationSpec {
            track_id: "camera".into(),
            kind: Some(wire::publication_spec::Kind::Video(
                wire::VideoPublication {
                    binding: Some(wire::TrackBinding {
                        transport: Some(wire::track_binding::Transport::Rtp(parameters)),
                    }),
                },
            )),
        }),
    )
    .await?;
    let (mut viewer, receiver, _, parameters) = connect(host.clone(), server, false, dd).await?;
    let receiver_fid = [
        parameters.encodings[0].ssrc.context("AV1 receiver SSRC")?,
        parameters.encodings[0]
            .rtx_ssrc
            .context("AV1 receiver RTX SSRC")?,
    ];
    let receiver_codec = parameters.codec.as_ref().context("AV1 receiver codec")?;
    let payload_types = [
        publisher_codec.payload_type.context("AV1 publisher PT")?,
        receiver_codec.payload_type.context("AV1 receiver PT")?,
        receiver_codec
            .rtx_payload_type
            .context("AV1 receiver RTX PT")?,
    ]
    .map(u8::try_from);
    let [ingress_pt, egress_pt, repair_pt] = payload_types;
    let (ingress_pt, egress_pt, repair_pt) = (ingress_pt?, egress_pt?, repair_pt?);
    if !dd {
        assert_eq!(publisher_codec.format, receiver_codec.format);
        assert_ne!(ingress_pt, egress_pt, "applied primary PTs did not differ");
        assert_ne!(
            publisher_codec.rtx_payload_type, receiver_codec.rtx_payload_type,
            "applied repair PTs did not differ"
        );
    }
    let remote_track = receiver.receiver().track().context("AV1 video track")?;
    let sink = remote_track.attach_sink()?;
    assert_eq!(
        install(
            &mut viewer,
            wire::resource_spec::Declaration::Subscription(wire::SubscriptionSpec {
                publication_id: track.publication_id.clone(),
                kind: wire::TrackKind::Video as i32,
                source_room: ROOM.into(),
                binding: Some(wire::TrackBinding {
                    transport: Some(wire::track_binding::Transport::Rtp(parameters)),
                }),
            }),
        )
        .await?,
        track
    );
    let counter = pulsebeam::sim_metrics::routing_counter;
    let enqueued = counter("fabric_rtp_enqueued");
    let shard_enqueued = counter("fabric_rtp_shard_enqueued");
    let source = match source {
        Source::Encoded(source) => source,
        Source::Stock(source) => {
            let carried_dd = counter("fabric_rtp_av1_dd_enqueued");
            let mut decoded = 0;
            let mut last_timestamp = None;
            for frame in 0..12_u32 {
                let mut pixels = vec![128; 1280 * 720 * 3 / 2];
                for (y, row) in pixels[..1280 * 720].chunks_exact_mut(1280).enumerate() {
                    for (x, pixel) in row.iter_mut().enumerate() {
                        *pixel = if (x / 64 + y / 48 + frame as usize / 4) % 2 == 0 {
                            52
                        } else {
                            172
                        };
                    }
                }
                source.push_frame(&VideoFrame::i420(
                    1280,
                    720,
                    pixels,
                    super::super::common::native::now_us()?,
                    frame * 3000,
                )?)?;
                tokio::time::sleep(Duration::from_millis(34)).await;
                while let Some(picture) = sink.try_next_frame() {
                    assert_eq!((picture.width, picture.height), (1280, 720));
                    assert_ne!(last_timestamp, Some(picture.rtp_timestamp));
                    last_timestamp = Some(picture.rtp_timestamp);
                    decoded += 1;
                }
            }
            anyhow::ensure!(
                decoded >= 2,
                "stock AV1 with negotiated DD did not decode: {decoded}"
            );
            assert!(
                counter("fabric_rtp_av1_dd_enqueued") > carried_dd,
                "no AV1 DD was admitted to the bounded RTP writer"
            );
            assert_eq!(
                counter("fabric_rtp_shard_enqueued") > shard_enqueued,
                cross_shard
            );
            return Ok(());
        }
    };
    let keyframe = host.av1_keyframe()?;
    let push = || -> anyhow::Result<()> {
        source.push_encoded(EncodedVideoAccessUnit {
            data: keyframe.clone(),
            width: 1280,
            height: 720,
            timestamp_us: super::super::common::native::now_us()?,
            key_frame: true,
            qp: None,
            metadata: EncodedVideoMetadata {
                codec: EncodedVideoCodec::Av1,
                simulcast_index: None,
                spatial_index: None,
                temporal_index: None,
                end_of_picture: true,
            },
        })?;
        Ok(())
    };
    // Send only one warmup frame so a delayed capture cannot satisfy the
    // next phase or interleave with its single-frame packet witness.
    tokio::time::sleep(Duration::from_millis(100)).await;
    push()?;
    let warmup = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(frame) = sink.try_next_frame() {
                return frame;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .context("no decoded Native AV1 after State FID adoption")?;
    assert_eq!((warmup.width, warmup.height), (1280, 720));
    let before_refresh = warmup.rtp_timestamp;
    assert!(counter("fabric_rtp_enqueued") > enqueued);
    assert_eq!(
        counter("fabric_rtp_shard_enqueued") > shard_enqueued,
        cross_shard
    );

    tokio::time::sleep(Duration::from_millis(600)).await;
    let last = std::iter::from_fn(|| sink.try_next_frame())
        .last()
        .map_or(before_refresh, |frame| frame.rtp_timestamp);
    let before = publisher_pli_count(&publisher.peer, publisher_ssrc).await?;
    receiver.receiver().request_keyframe()?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = publisher_pli_count(&publisher.peer, publisher_ssrc).await?;
    anyhow::ensure!(
        after > before,
        "Native AV1 publisher PLI count did not increase: {before} -> {after}"
    );
    let udp = host.observe_udp_rtp(server);
    udp.drop_second_primary(publisher_ssrc, receiver_fid);
    push()?;
    let refreshed = tokio::time::timeout(Duration::from_secs(1), async {
        let mut decoded = None;
        loop {
            if let Some(timestamp) = udp.repaired_timestamp() {
                while let Some(frame) = sink.try_next_frame() {
                    if frame.rtp_timestamp == timestamp {
                        decoded = Some(frame);
                    }
                }
            }
            // A receiver-local repair can overtake an original datagram on
            // this reordered path. Decode and primary arrival are separate.
            if udp.saw_all_primary_fragments() && let Some(frame) = decoded.take() {
                return frame;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let proof = udp
        .finish_repair()
        .context("protected AV1 repair witness")?;
    let refreshed = refreshed.with_context(|| {
        format!(
            "Native AV1 did not complete repaired decode and primary packet witness: {proof:?}, UDP peaks={:?}",
            udp.peaks()
        )
    })?;
    assert_ne!(refreshed.rtp_timestamp, last);
    assert_eq!((refreshed.width, refreshed.height), (1280, 720));
    assert!(
        proof
            .input
            .iter()
            .all(|packet| packet.payload_type == ingress_pt)
    );
    assert!(
        proof
            .output
            .iter()
            .all(|packet| packet.payload_type == egress_pt)
    );
    assert!(
        proof
            .repairs
            .iter()
            .all(|packet| packet.payload_type == repair_pt)
    );
    let dropped = proof.dropped.with_context(|| {
        format!(
            "lost interior AV1 primary: {proof:?}, UDP peaks={:?}",
            udp.peaks()
        )
    })?;
    assert!(proof.output.len() >= 3);
    assert_eq!(proof.input.len(), proof.output.len(), "{proof:?}");
    for leg in [&proof.input, &proof.output] {
        // Network delivery can reorder fragments. The marker still terminates
        // one complete contiguous primary sequence on each leg.
        let mut markers = leg.iter().filter(|packet| packet.marker);
        let end = markers.next().context("AV1 frame marker")?;
        assert!(markers.next().is_none());
        let mut offsets = leg
            .iter()
            .map(|packet| {
                assert_eq!(packet.timestamp, end.timestamp);
                end.sequence.wrapping_sub(packet.sequence)
            })
            .collect::<Vec<_>>();
        offsets.sort_unstable();
        assert!(offsets.iter().copied().eq(0..u16::try_from(leg.len())?));
    }
    const UDP_PAYLOAD_CAP: usize = 1_232;
    assert!(
        proof
            .output
            .iter()
            .all(|packet| packet.bytes <= UDP_PAYLOAD_CAP)
    );
    // Repeated Native NACKs may cause several copies before decode completes.
    assert!(!proof.repairs.is_empty());
    assert!(
        proof
            .repairs
            .iter()
            .all(|packet| packet.bytes == dropped + 2)
    );
    assert!(
        proof
            .repairs
            .iter()
            .all(|packet| packet.bytes <= UDP_PAYLOAD_CAP)
    );
    Ok(())
}

#[test]
fn native_state_av1_decodes_with_adopted_fid() {
    prove_native_state_av1(1, false);
}

#[test]
fn native_state_av1_decodes_with_adopted_fid_across_shards() {
    prove_native_state_av1(2, false);
}

#[test]
fn native_state_av1_carries_dd_without_selection() {
    prove_native_state_av1(1, true);
}

#[test]
fn native_state_av1_carries_dd_without_selection_across_shards() {
    prove_native_state_av1(2, true);
}

fn prove_native_state_av1(shards: usize, dd: bool) {
    let seed = std::env::var("PULSEBEAM_SIM_SEED").map_or(1, |seed| seed.parse().unwrap());
    let _clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    pulsebeam_runtime::net::shaper::seed_impairments(seed);
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(8))
        .rng_seed(seed)
        .build();
    let subnet = reserve_subnet();
    let server = subnet_ip(subnet, 1);
    let client = subnet_ip(subnet, 2);
    sim.host(server, move || async move {
        sfu_node_builder(server, pulsebeam_runtime::rand::seeded_rng(seed), shards)
            .run(tokio_util::sync::CancellationToken::new())
            .await
            .map_err(Into::into)
    });
    sim.client(client, async move {
        session(NativeHost::bind(client)?, server, shards > 1, dd).await?;
        Ok(())
    });
    run_sim_or_timeout(&mut sim, Duration::from_secs(30)).expect("managed Native AV1 replay");
}
