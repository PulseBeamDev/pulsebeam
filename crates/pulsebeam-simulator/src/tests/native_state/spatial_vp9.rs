use std::net::SocketAddr;

use pulsebeam_agent_core::client_sdp as client;
use pulsebeam_rtc_simulation::{
    MediaType, RtpHeaderExtensionDirection, RtpTransceiverDirection, VideoSink,
};

use super::*;

pub(in crate::tests) enum Check {
    WanRepair {
        publisher: &'static str,
        track: &'static str,
        edge: (SocketAddr, SocketAddr),
    },
    ColdStart {
        publisher: &'static str,
        track: &'static str,
        prove_relay_projection: bool,
        shared_hop: Option<(SocketAddr, SocketAddr)>,
    },
    HeightChanges {
        sustained: bool,
    },
    Bandwidth {
        receiver_ip: IpAddr,
    },
    CapacityTracking {
        receiver_ip: IpAddr,
        profile: CapacityTracking,
    },
    ResizeWithScreen {
        screen: String,
    },
    Contention {
        receiver_ip: IpAddr,
        other: String,
    },
}

#[derive(Clone, Copy)]
pub(in crate::tests) enum CapacityTracking {
    Ramp,
    Oscillation,
}

async fn connect_viewer(
    address: SocketAddr,
    host: NativeHost,
    subject: &str,
    codec_names: &[&str],
) -> anyhow::Result<(WirePeer, Vec<(wire::TrackBinding, VideoSink)>)> {
    let mut transceivers = Vec::with_capacity(codec_names.len());
    let (mut viewer, _) = WirePeer::connect_with_peer(
        host,
        address,
        subject,
        "stage",
        true,
        Some(VideoBootstrap::Vp9Receiver),
        |factory, peer| {
            let source = factory.create_video_source()?;
            let track = factory.create_video_track("vp9", &source)?;
            let codecs = factory
                .get_rtp_sender_capabilities(MediaType::Video)
                .codecs();
            let codec = |name: &[u8]| {
                codecs
                    .iter()
                    .find(|codec| {
                        let base = codec.base_codec();
                        base.name_bytes() == name
                            && (name != b"H264"
                                || (base.parameters().iter().any(|parameter| {
                                    parameter.key == b"profile-level-id"
                                        && parameter.value == b"42e01f"
                                }) && base.parameters().iter().any(|parameter| {
                                    parameter.key == b"packetization-mode"
                                        && parameter.value == b"1"
                                })))
                    })
                    .cloned()
                    .with_context(|| format!("missing stock codec capability for {name:?}"))
            };
            let rtx = codec(b"rtx")?;
            for name in codec_names {
                let rtp_name: &[u8] = if *name == "vp9" { b"VP9" } else { b"H264" };
                let codecs = [codec(rtp_name)?, rtx.clone()];
                let receiver =
                    peer.add_video_transceiver(&track, RtpTransceiverDirection::ReceiveOnly)?;
                receiver.set_rtp_codec_preferences(&codecs)?;
                let mut extensions = receiver.header_extensions_to_negotiate()?;
                for extension in &mut extensions {
                    if extension.uri() == pulsebeam_core::dd::URI {
                        extension.direction = RtpHeaderExtensionDirection::ReceiveOnly;
                    }
                }
                receiver.set_header_extensions_to_negotiate(&extensions)?;
                transceivers.push(receiver);
            }
            Ok(())
        },
    )
    .await?;
    let answer = viewer
        .peer
        .descriptions()?
        .current_remote
        .context("applied VP9 answer")?;
    let requests = client::inspect(&answer.sdp)?
        .into_iter()
        .filter(|section| section.kind == client::Kind::Video)
        .zip(codec_names)
        .enumerate()
        .map(|(index, (section, name))| {
            let codec = section
                .codecs
                .iter()
                .find(|codec| &codec.parameters.name == name)
                .with_context(|| format!("missing negotiated video codec {name}"))?;
            Ok(client::Preparation {
                mid: section.mid,
                payload_type: codec.payload_type,
                publication: None,
                reception: Some(client::MediaOwner {
                    key: client::MediaKey(format!("vp9-slot-{index}")),
                    group: None,
                }),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let adopted = viewer
        .media
        .as_mut()
        .context("native media context")?
        .adopt_bootstrap_media(&requests)?;
    anyhow::ensure!(
        adopted.receptions.len() == codec_names.len(),
        "missing negotiated VP9 receive slots"
    );
    let outputs = transceivers
        .iter()
        .zip(&adopted.receptions)
        .zip(codec_names)
        .map(|((transceiver, binding), name)| {
            anyhow::ensure!(
                binding.complete()
                    && (*name != "vp9"
                        || binding
                            .extensions
                            .iter()
                            .any(|extension| extension.uri == pulsebeam_core::dd::URI)),
                "incomplete negotiated video binding: {binding:?}"
            );
            let remote = transceiver
                .receiver()
                .track()
                .context("VP9 receive track")?;
            Ok((
                wire::TrackBinding {
                    transport: Some(wire::track_binding::Transport::Rtp(av1::video_parameters(
                        binding,
                    )?)),
                },
                remote.attach_sink()?,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((viewer, outputs))
}

async fn prove_wan_impairment(
    (from, to): (std::net::SocketAddr, std::net::SocketAddr),
    sink: &VideoSink,
    wait: Duration,
) -> anyhow::Result<()> {
    use pulsebeam_runtime::net::shaper::{
        Capacity, DirectedLink, Loss, Reorder, directed_stats, remove_directed_link,
        set_directed_link,
    };
    let before = directed_stats(from, to);
    let reverse_before = directed_stats(to, from);
    set_directed_link(
        from,
        to,
        DirectedLink {
            capacity: Capacity::Fixed(20_000_000),
            max_backlog: Duration::from_millis(20),
            delay: Duration::from_millis(12),
            loss: Loss::Burst {
                to_bad: 0.12,
                to_good: 0.55,
                loss_in_good: 0.0,
                loss_in_bad: 0.55,
            },
            reorder: Reorder {
                probability: 0.12,
                delay: Duration::from_millis(18),
            },
        },
    );
    let impaired = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let stats = directed_stats(from, to);
            if stats.dropped_loss > before.dropped_loss
                && stats.reordered > before.reordered
                && stats.admitted_udp_bytes > before.admitted_udp_bytes
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    remove_directed_link(from, to);
    impaired.with_context(|| {
        format!(
            "forward WAN impairment was not exercised: {:?}",
            directed_stats(from, to)
        )
    })?;
    anyhow::ensure!(
        directed_stats(to, from) == reverse_before,
        "forward WAN impairment also shaped the reverse path"
    );
    while sink.try_next_frame().is_some() {}
    expect_height(
        sink,
        "WAN recovery after burst and jitter",
        720,
        wait,
        false,
    )
    .await
}

pub(in crate::tests) async fn expect_height(
    sink: &VideoSink,
    label: &str,
    height: u32,
    wait: Duration,
    strict: bool,
) -> anyhow::Result<()> {
    use pulsebeam_runtime::time::Instant;
    let mut run = 0;
    let mut heights = [0; 3];
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        while let Some(frame) = sink.try_next_frame() {
            let index = match frame.height {
                180 => 0,
                360 => 1,
                720 => 2,
                other => anyhow::bail!("{label}: unexpected video frame height {other}"),
            };
            heights[index] += 1;
            if frame.height == height {
                run += 1;
            } else {
                anyhow::ensure!(
                    !strict,
                    "{label}: decoded {}p, expected {height}p",
                    frame.height
                );
                run = 0;
            }
        }
        if run >= 10 {
            tracing::debug!(label, height, ?heights, "VP9 decoded height sustained");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    anyhow::bail!("{label}: never sustained {height}p; frames by height: {heights:?}")
}

pub(in crate::tests) async fn receive(
    address: SocketAddr,
    host: NativeHost,
    publication: &str,
    check: Check,
    start_replay: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if let Check::ResizeWithScreen { screen } = &check {
        return check_resize_with_screen(address, host, publication, screen, start_replay).await;
    }
    if let Check::Contention { receiver_ip, other } = &check {
        return check_contention(
            address,
            host,
            *receiver_ip,
            [publication, other],
            start_replay,
        )
        .await;
    }
    // An exact RTP binding is single-use. Reserve a second negotiated receive
    // slot so logical recovery does not require reopening the PeerConnection.
    let slots = if matches!(check, Check::WanRepair { .. }) {
        2
    } else {
        1
    };
    let (mut viewer, outputs) =
        connect_viewer(address, host, "vp9-viewer", &vec!["vp9"; slots]).await?;
    let mut outputs = outputs.into_iter();
    let (binding, sink) = outputs.next().context("missing initial VP9 slot")?;
    let declaration = match check {
        Check::WanRepair {
            publisher, track, ..
        }
        | Check::ColdStart {
            publisher, track, ..
        } => wire::resource_spec::Declaration::LogicalSubscription(wire::LogicalSubscriptionSpec {
            source: Some(wire::LogicalSourceAddress {
                room: "stage".into(),
                participant_id: publisher.into(),
                kind: wire::TrackKind::Video as i32,
                track_id: track.into(),
            }),
        }),
        _ => wire::resource_spec::Declaration::Subscription(wire::SubscriptionSpec {
            publication_id: publication.into(),
            kind: wire::TrackKind::Video as i32,
            source_room: "stage".into(),
            binding: Some(binding.clone()),
        }),
    };
    let reference = 1;
    let mut spec = wire::ResourceSpec {
        declaration: Some(declaration),
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: Some(wire::resource_settings::Receive::Video(
                wire::VideoReceiveOptions {
                    max_height: matches!(
                        check,
                        Check::HeightChanges { .. }
                            | Check::ColdStart {
                                prove_relay_projection: true,
                                ..
                            }
                    )
                    .then_some(180),
                    ..Default::default()
                },
            )),
        }),
    };
    let first_source = if matches!(check, Check::WanRepair { .. } | Check::ColdStart { .. }) {
        viewer.desire(reference, reference, Some(spec.clone()));
        Some(
            tokio::time::timeout(
                Duration::from_secs(8),
                bind_logical_video(&mut viewer, publication, &binding, 2, 0),
            )
            .await
            .context("VP9 logical subscription did not install")??,
        )
    } else {
        install(&mut viewer, reference, reference, &spec).await?;
        None
    };
    start_replay()?;
    match check {
        Check::WanRepair { edge, .. } => {
            use pulsebeam::sim_metrics::wan_repair;
            let wait = Duration::from_secs(5);
            expect_height(&sink, "WAN delivery", 720, wait, false).await?;
            wan_repair::arm_vp9(edge.0, edge.1);
            tokio::time::timeout(wait, async {
                while !wan_repair::report()
                    .is_some_and(|report| report.late_original_offers.is_some())
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .with_context(|| {
                format!(
                    "WAN VP9 exact repair did not converge: {:?}",
                    wan_repair::report()
                )
            })?;
            // Do not count decoded frames buffered before the recovery check.
            while sink.try_next_frame().is_some() {}
            expect_height(&sink, "WAN recovery after loss", 720, wait, false).await?;
            let report = wan_repair::report().context("missing WAN VP9 repair report")?;
            anyhow::ensure!(
                report
                    .opened_repair
                    .is_some_and(|label| label != report.live)
                    && report.repaired_timestamp == Some(report.live_wire_timestamp)
                    && report.late_original_matches == Some(true)
                    && report.repair_admitted,
                "WAN repair did not return the exact VP9 packet to fanout: {report:?}"
            );
            let cursor = report
                .dd_cursor_before_first_offer
                .context("missing VP9 DD cursor at repair")?;
            if cursor >= report.position {
                anyhow::ensure!(
                    report.rtc_enqueues == 0,
                    "late VP9 repair rewound output: {report:?}"
                );
            } else {
                anyhow::ensure!(
                    report.rtc_enqueues == 1,
                    "in-order VP9 repair did not reach output exactly once: {report:?}"
                );
            }

            prove_wan_impairment(edge, &sink, wait).await?;

            // Cut only publisher-node to viewer-node traffic. Both RTC legs and
            // the reverse inter-node direction remain reachable.
            turmoil::partition_oneway(edge.0.ip(), edge.1.ip());
            tokio::time::sleep(Duration::from_millis(500)).await;
            while sink.try_next_frame().is_some() {}
            tokio::time::sleep(Duration::from_millis(400)).await;
            anyhow::ensure!(
                sink.try_next_frame().is_none(),
                "viewer decoded during the inter-node cut"
            );
            turmoil::repair_oneway(edge.0.ip(), edge.1.ip());
            // A new association can withdraw and re-present the same publication.
            // Keep the application intent and PeerConnection, but install the new
            // physical binding only after the new source revision is offered.
            let (binding, replacement_sink) =
                outputs.next().context("missing replacement VP9 slot")?;
            tokio::time::timeout(Duration::from_secs(21), async {
                bind_logical_video(
                    &mut viewer,
                    publication,
                    &binding,
                    3,
                    first_source.context("missing original source revision")?,
                )
                .await?;
                expect_height(
                    &replacement_sink,
                    "WAN recovery after directed partition",
                    720,
                    wait,
                    false,
                )
                .await
            })
            .await
            .context(
                "WAN VP9 logical subscription did not recover within the lease and reopen budget",
            )?
        }
        Check::ColdStart {
            prove_relay_projection,
            shared_hop,
            ..
        } => {
            if prove_relay_projection {
                let counter =
                    || pulsebeam::sim_metrics::routing_counter("fabric_wan_relay_projected_runs");
                let before = counter();
                let wait = Duration::from_secs(8);
                expect_height(&sink, "regional cold VP9 at base", 180, wait, true).await?;
                tokio::time::timeout(wait, async {
                    while counter() == before {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .context("regional egress never projected the authorized base demand")?;
                expect_height(&sink, "projected regional VP9 base", 180, wait, true).await?;
                spec.settings = Some(wire::ResourceSettings {
                    enabled: Some(true),
                    receive: Some(wire::resource_settings::Receive::Video(
                        wire::VideoReceiveOptions {
                            max_height: Some(720),
                            ..Default::default()
                        },
                    )),
                });
                use wire::logical_subscription_state::Execution;
                let revision = 3; // Revision 2 was the prepared receiver report.
                let source_revision = first_source.context("missing original source revision")?;
                let installed = Execution::Installed(binding.clone());
                viewer.desire(reference, revision, Some(spec.clone()));
                tokio::time::timeout(
                    wait,
                    viewer.logical_when(revision, |state| {
                        state.spec.as_ref() == Some(&spec)
                            && state.source_revision == source_revision
                            && state.execution.as_ref() == Some(&installed)
                    }),
                )
                .await
                .context("regional VP9 promotion did not retain its original binding")??;
            }
            expect_height(
                &sink,
                "remote-only VP9 startup",
                720,
                Duration::from_secs(10),
                false,
            )
            .await?;
            if let Some(edge) = shared_hop {
                let worst_gap = check_shared_hop_squeeze(&sink, edge).await?;
                expect_height(
                    &sink,
                    "shared-hop recovery",
                    720,
                    Duration::from_secs(20),
                    false,
                )
                .await
                .with_context(|| {
                    format!("maximum decoded VP9 gap during squeeze: {worst_gap:?}")
                })?;
                anyhow::ensure!(
                    worst_gap < Duration::from_millis(450),
                    "VP9 playback stalled during the WAN squeeze: gap={worst_gap:?}"
                );
            }
            Ok(())
        }
        Check::HeightChanges { sustained } => {
            let wait = Duration::from_secs(3);
            expect_height(&sink, "initial height ceiling", 180, wait, true).await?;
            if sustained {
                let low = tokio::time::Instant::now() + Duration::from_secs(60);
                while tokio::time::Instant::now() < low {
                    expect_height(&sink, "sustained low target", 180, wait, true).await?;
                }
            }
            for (revision, ceiling, height) in [
                (2, 720, 720),
                (3, 360, 360),
                (4, 540, 360),
                (5, 180, 180),
                (6, 720, 720),
            ] {
                spec.settings = Some(wire::ResourceSettings {
                    enabled: Some(true),
                    receive: Some(wire::resource_settings::Receive::Video(
                        wire::VideoReceiveOptions {
                            max_height: Some(ceiling),
                            ..Default::default()
                        },
                    )),
                });
                install(&mut viewer, reference, revision, &spec).await?;
                expect_height(&sink, "updated height ceiling", height, wait, false).await?;
                if ceiling != height {
                    expect_height(&sink, "between-layer ceiling", height, wait, true).await?;
                }
            }
            if !sustained {
                expect_height(&sink, "independent top target holds", 720, wait, true).await?;
                return Ok(());
            }
            let mut frames = 0;
            for _ in 0..5 {
                let mut batch = 0;
                let window = tokio::time::Instant::now() + Duration::from_secs(3);
                while tokio::time::Instant::now() < window {
                    while let Some(frame) = sink.try_next_frame() {
                        anyhow::ensure!(
                            frame.height == 720,
                            "top target did not hold after upgrade"
                        );
                        batch += 1;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                anyhow::ensure!(batch > 0, "no fresh decoded video after upgrade");
                frames += batch;
            }
            anyhow::ensure!(frames >= 200, "only {frames} decoded frames after upgrade");
            Ok(())
        }
        Check::Bandwidth { receiver_ip } => {
            use pulsebeam_runtime::net::shaper::{reset_stats_for, set_downlink, stats};
            let wait = Duration::from_secs(9);
            const TOP_BPS: u64 = 3_000_000;
            set_downlink(receiver_ip, TOP_BPS);
            expect_height(&sink, "cold top target at 3 Mbps", 720, wait, false).await?;
            reset_stats_for([receiver_ip]);
            expect_height(&sink, "sustained top target at 3 Mbps", 720, wait, true).await?;
            let top = stats(receiver_ip);
            anyhow::ensure!(
                top.admitted_udp_bytes > 0 && top.mean_backlog() < Duration::from_millis(100),
                "720p did not traverse a bounded receiver queue: {top:?}"
            );
            set_downlink(receiver_ip, 650_000);
            let before = stats(receiver_ip);
            expect_height(&sink, "constrained", 180, wait, false)
                .await
                .with_context(|| {
                    let after = stats(receiver_ip);
                    format!(
                        "VP9 downlink counters at {receiver_ip}: before={before:?}; after={after:?}"
                    )
                })?;
            anyhow::ensure!(
                stats(receiver_ip).delivered > before.delivered,
                "RTC traffic did not traverse the constrained downlink"
            );
            expect_height(&sink, "sustained constrained target", 180, wait, true).await?;
            set_downlink(receiver_ip, TOP_BPS);
            // RTC probes upward from the constrained rate instead of jumping
            // straight to the full encoding cost when capacity returns.
            expect_height(
                &sink,
                "back to 720p at 3 Mbps",
                720,
                Duration::from_secs(22),
                false,
            )
            .await?;
            expect_height(&sink, "sustained 720p after recovery", 720, wait, true).await
        }
        Check::CapacityTracking {
            receiver_ip,
            profile,
        } => track_capacity(&sink, receiver_ip, profile).await,
        Check::Contention { .. } | Check::ResizeWithScreen { .. } => {
            unreachable!("two-source scenarios bind independent slots")
        }
    }
}

async fn check_resize_with_screen(
    address: SocketAddr,
    host: NativeHost,
    camera: &str,
    screen: &str,
    start_replay: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let (mut viewer, mut outputs) =
        connect_viewer(address, host, "vp9-viewer", &["vp9", "h264"]).await?;
    let (screen_binding, screen_sink) = outputs.pop().context("missing screen decoder")?;
    let (camera_binding, camera_sink) = outputs.pop().context("missing camera decoder")?;
    let video = |publication: &str, binding, height| wire::ResourceSpec {
        declaration: Some(wire::resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                publication_id: publication.to_owned(),
                source_room: "stage".into(),
                kind: wire::TrackKind::Video as i32,
                binding: Some(binding),
            },
        )),
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: Some(wire::resource_settings::Receive::Video(
                wire::VideoReceiveOptions {
                    max_height: Some(height),
                    ..Default::default()
                },
            )),
        }),
    };
    install(
        &mut viewer,
        1,
        1,
        &video(camera, camera_binding.clone(), 720),
    )
    .await?;
    install(&mut viewer, 2, 2, &video(screen, screen_binding, 1080)).await?;
    start_replay()?;

    let wait = Duration::from_secs(12);
    let screen_progress = |phase| {
        let screen_sink = &screen_sink;
        async move {
            let mut frames = 0;
            let progress = tokio::time::timeout(wait, async {
                while frames < 3 {
                    while let Some(frame) = screen_sink.try_next_frame() {
                        anyhow::ensure!(
                            frame.width >= 1280 && frame.height >= 720,
                            "screen lost full-size playback: {}x{}",
                            frame.width,
                            frame.height,
                        );
                        frames += 1;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok(())
            })
            .await;
            progress
                .with_context(|| format!("only {frames} fresh full-size screen frames {phase}"))?
        }
    };
    tokio::try_join!(
        expect_height(&camera_sink, "camera before resize", 720, wait, false),
        screen_progress("before resize"),
    )?;

    install(&mut viewer, 1, 3, &video(camera, camera_binding, 360)).await?;
    expect_height(&camera_sink, "resized camera", 360, wait, false).await?;
    while screen_sink.try_next_frame().is_some() {}
    tokio::try_join!(
        expect_height(&camera_sink, "camera holds resized height", 360, wait, true),
        screen_progress("after resize"),
    )?;
    Ok(())
}

async fn check_contention(
    address: SocketAddr,
    host: NativeHost,
    receiver_ip: IpAddr,
    publications: [&str; 2],
    start_replay: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use pulsebeam_runtime::net::shaper::{reset_stats_for, set_downlink, stats};

    let (mut viewer, mut outputs) =
        connect_viewer(address, host, "vp9-viewer", &["vp9"; 2]).await?;
    let (protected_binding, protected) = outputs.pop().context("missing protected decoder")?;
    let (focused_binding, focused) = outputs.pop().context("missing focused decoder")?;
    let subscription = |publication: &str, binding, priority, min_height| wire::ResourceSpec {
        declaration: Some(wire::resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                publication_id: publication.to_owned(),
                kind: wire::TrackKind::Video as i32,
                source_room: "stage".into(),
                binding: Some(binding),
            },
        )),
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: Some(wire::resource_settings::Receive::Video(
                wire::VideoReceiveOptions {
                    min_height: (min_height > 0).then_some(min_height),
                    max_height: Some(720),
                    priority: Some(priority),
                    ..Default::default()
                },
            )),
        }),
    };
    set_downlink(receiver_ip, 7_000_000);
    install(
        &mut viewer,
        1,
        1,
        &subscription(publications[0], focused_binding.clone(), 200, 180),
    )
    .await?;
    install(
        &mut viewer,
        2,
        2,
        &subscription(publications[1], protected_binding.clone(), 10, 180),
    )
    .await?;
    start_replay()?;
    let wait = Duration::from_secs(10);
    tokio::try_join!(
        expect_height(&focused, "focused before squeeze", 720, wait, false),
        expect_height(&protected, "protected before squeeze", 720, wait, false),
    )?;
    reset_stats_for([receiver_ip]);
    set_downlink(receiver_ip, 650_000);
    tokio::try_join!(
        expect_height(&focused, "focused floor under contention", 180, wait, false),
        expect_height(
            &protected,
            "lower-priority floor under contention",
            180,
            wait,
            false
        ),
    )?;
    let constrained = stats(receiver_ip);
    anyhow::ensure!(
        constrained.admitted_udp_bytes > 0,
        "squeeze never shaped the receiver: {constrained:?}"
    );

    // The captured ladder costs about 282 kbps at 180p and 1 Mbps at 360p.
    // Leave estimator headroom for one 360p reservation plus one base while
    // keeping the link below even the payload cost of two 360p targets.
    set_downlink(receiver_ip, 1_900_000);
    install(
        &mut viewer,
        2,
        3,
        &subscription(publications[1], protected_binding.clone(), 10, 360),
    )
    .await?;
    while protected.try_next_frame().is_some() {}
    expect_height(
        &protected,
        "lower-priority 360p reservation",
        360,
        Duration::from_secs(14),
        false,
    )
    .await?;
    while focused.try_next_frame().is_some() {}
    tokio::try_join!(
        expect_height(
            &focused,
            "high-priority upgrade yields to the floor",
            180,
            wait,
            true
        ),
        expect_height(&protected, "360p reservation holds", 360, wait, true),
    )?;

    // At the same bottleneck, both now reserve 360p. Only the priority
    // order changes between the next two observations.
    install(
        &mut viewer,
        1,
        4,
        &subscription(publications[0], focused_binding.clone(), 200, 360),
    )
    .await?;
    expect_height(&focused, "higher-priority 360p minimum", 360, wait, false).await?;
    while protected.try_next_frame().is_some() {}
    expect_height(&protected, "lower-priority fallback", 180, wait, false).await?;

    install(
        &mut viewer,
        1,
        5,
        &subscription(publications[0], focused_binding.clone(), 10, 360),
    )
    .await?;
    install(
        &mut viewer,
        2,
        6,
        &subscription(publications[1], protected_binding.clone(), 200, 360),
    )
    .await?;
    expect_height(
        &protected,
        "360p minimum after priority reversal",
        360,
        wait,
        false,
    )
    .await?;
    while focused.try_next_frame().is_some() {}
    tokio::try_join!(
        expect_height(
            &focused,
            "fallback after priority reversal",
            180,
            wait,
            false
        ),
        expect_height(&protected, "new winner sustains 360p", 360, wait, true),
    )?;

    // One 720p target fits here. Removing the background reservation must
    // release its retained top rung to the newly focused subscription.
    set_downlink(receiver_ip, 2_800_000);
    install(
        &mut viewer,
        2,
        7,
        &subscription(publications[1], protected_binding.clone(), 200, 720),
    )
    .await?;
    expect_height(
        &protected,
        "background holds the only top rung",
        720,
        wait,
        false,
    )
    .await?;
    install(
        &mut viewer,
        2,
        8,
        &subscription(publications[1], protected_binding, 10, 0),
    )
    .await?;
    install(
        &mut viewer,
        1,
        9,
        &subscription(publications[0], focused_binding, 200, 360),
    )
    .await?;
    while focused.try_next_frame().is_some() {}
    expect_height(
        &focused,
        "focus reclaims the only top rung",
        720,
        wait,
        false,
    )
    .await?;
    expect_height(&focused, "focused top rung holds", 720, wait, true).await?;

    set_downlink(receiver_ip, 7_000_000);
    tokio::try_join!(
        expect_height(
            &focused,
            "focused after recovery",
            720,
            Duration::from_secs(22),
            false
        ),
        expect_height(
            &protected,
            "protected after recovery",
            720,
            Duration::from_secs(22),
            false
        ),
    )?;
    Ok(())
}

async fn track_capacity(
    sink: &VideoSink,
    receiver_ip: IpAddr,
    profile: CapacityTracking,
) -> anyhow::Result<()> {
    use pulsebeam_runtime::net::shaper::{
        Capacity, Loss, reset_stats_for, set_capacity, set_downlink, set_loss, stats,
    };
    let wait = Duration::from_secs(9);
    let (start, capacity, settle, window, congestion_percent) = match profile {
        CapacityTracking::Ramp => (
            3_000_000,
            Capacity::Ramp {
                from: 3_000_000,
                to: 650_000,
                over: Duration::from_secs(20),
            },
            Duration::from_secs(25),
            Duration::from_secs(5),
            1,
        ),
        CapacityTracking::Oscillation => (
            2_500_000,
            Capacity::Oscillate {
                min: 800_000,
                max: 2_500_000,
                period: Duration::from_secs(10),
            },
            Duration::from_secs(10),
            Duration::from_secs(30),
            8,
        ),
    };
    set_downlink(receiver_ip, start);
    expect_height(
        sink,
        "top target before capacity tracking",
        720,
        wait,
        false,
    )
    .await?;
    set_capacity(receiver_ip, capacity, Duration::from_millis(200));
    if matches!(profile, CapacityTracking::Oscillation) {
        set_loss(receiver_ip, Loss::wifi());
    }
    tokio::time::sleep(settle).await;
    if matches!(profile, CapacityTracking::Ramp) {
        expect_height(sink, "bottom target after ramp", 180, wait, false).await?;
    }
    while sink.try_next_frame().is_some() {}
    reset_stats_for([receiver_ip]);
    let deadline = tokio::time::Instant::now() + window;
    let mut frames = 0;
    while tokio::time::Instant::now() < deadline {
        while sink.try_next_frame().is_some() {
            frames += 1;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let path = stats(receiver_ip);
    set_loss(receiver_ip, Loss::NONE);
    let offered = path.delivered + path.dropped_overflow;
    anyhow::ensure!(
        path.admitted_udp_bytes > 0 && frames >= 10,
        "no live shaped video: {frames} frames, {path:?}"
    );
    anyhow::ensure!(
        path.dropped_overflow * 100 < offered * congestion_percent,
        "sustained congestion loss: {path:?}"
    );
    match profile {
        CapacityTracking::Ramp => anyhow::ensure!(
            path.mean_backlog() < Duration::from_millis(100),
            "standing queue after ramp: {path:?}"
        ),
        CapacityTracking::Oscillation => anyhow::ensure!(
            path.dropped_loss > 0,
            "burst model did not hit the receiver: {path:?}"
        ),
    }
    Ok(())
}

pub(in crate::tests) async fn check_viewer_isolation(
    server: SocketAddr,
    host: NativeHost,
    publication: &str,
    wan: (SocketAddr, SocketAddr),
) -> anyhow::Result<()> {
    use pulsebeam_runtime::net::shaper::{
        Capacity, directed_stats, remove_directed_link, set_directed_capacity,
    };
    use pulsebeam_runtime::time::Instant;

    let udp = host.observe_udp_rtp(server.ip());
    let connect = async |subject: &str, max_height| {
        let (mut viewer, outputs) = connect_viewer(server, host.clone(), subject, &["vp9"]).await?;
        let (binding, sink) = outputs.into_iter().next().context("VP9 receive slot")?;
        let Some(wire::track_binding::Transport::Rtp(rtp)) = &binding.transport else {
            anyhow::bail!("VP9 viewer did not bind RTP");
        };
        let ssrc = rtp
            .encodings
            .first()
            .and_then(|encoding| encoding.ssrc)
            .context("VP9 viewer primary SSRC")?;
        udp.watch_receive_edge(ssrc);
        viewer.desire(
            1,
            1,
            Some(wire::ResourceSpec {
                declaration: Some(wire::resource_spec::Declaration::LogicalSubscription(
                    wire::LogicalSubscriptionSpec {
                        source: Some(wire::LogicalSourceAddress {
                            room: "stage".into(),
                            participant_id: "vp9-source".into(),
                            kind: wire::TrackKind::Video as i32,
                            track_id: "spatial-camera".into(),
                        }),
                    },
                )),
                settings: Some(wire::ResourceSettings {
                    enabled: Some(true),
                    receive: Some(wire::resource_settings::Receive::Video(
                        wire::VideoReceiveOptions {
                            max_height: Some(max_height),
                            ..Default::default()
                        },
                    )),
                }),
            }),
        );
        bind_logical_video(&mut viewer, publication, &binding, 2, 0).await?;
        Ok::<_, anyhow::Error>((viewer, sink, ssrc))
    };
    let wait = Duration::from_secs(10);
    let ((_low, low_sink, low_ssrc), (_high, high_sink, high_ssrc)) =
        tokio::time::timeout(wait, async {
            tokio::try_join!(
                connect("vp9-viewer-low", 180),
                connect("vp9-viewer-high", 720)
            )
        })
        .await
        .context("install two logical VP9 viewers")??;
    tokio::try_join!(
        expect_height(&low_sink, "initial low ceiling", 180, wait, true),
        expect_height(&high_sink, "initial high ceiling", 720, wait, false),
    )?;
    let low_edge = udp.receive_edge(low_ssrc).context("low VP9 socket edge")?;
    let high_edge = udp
        .receive_edge(high_ssrc)
        .context("high VP9 socket edge")?;
    anyhow::ensure!(
        low_edge != high_edge && low_edge != wan && high_edge != wan,
        "shared hop and both endpoint legs must be distinct: {wan:?}, {low_edge:?}, {high_edge:?}"
    );
    let low_frames = std::cell::Cell::new(0usize);
    let check_low = async {
        let mut last = Instant::now();
        loop {
            while let Some(frame) = low_sink.try_next_frame() {
                anyhow::ensure!(frame.height == 180, "unshaped viewer exceeded its ceiling");
                last = Instant::now();
                low_frames.set(low_frames.get() + 1);
            }
            anyhow::ensure!(
                last.elapsed() < Duration::from_millis(450),
                "unshaped viewer stalled: last frame {last:?}, shared path {:?}",
                directed_stats(wan.0, wan.1)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let phases = async {
        let gap = check_shared_hop_squeeze(&high_sink, wan).await?;
        anyhow::ensure!(
            gap < Duration::from_millis(450),
            "shared-hop high viewer stalled: {gap:?}"
        );
        expect_height(
            &high_sink,
            "shared-hop recovery",
            720,
            Duration::from_secs(20),
            false,
        )
        .await?;
        while high_sink.try_next_frame().is_some() {}
        let before = directed_stats(high_edge.0, high_edge.1);
        let low_before = directed_stats(low_edge.0, low_edge.1);
        let wan_before = directed_stats(wan.0, wan.1);
        let count = low_frames.get();
        set_directed_capacity(
            high_edge.0,
            high_edge.1,
            Capacity::Fixed(650_000),
            Duration::from_millis(100),
        );
        expect_height(&high_sink, "isolated high viewer squeeze", 180, wait, false)
            .await
            .with_context(|| {
                format!(
                    "endpoint {high_edge:?}: {before:?} -> {:?}",
                    directed_stats(high_edge.0, high_edge.1)
                )
            })?;
        let after = directed_stats(high_edge.0, high_edge.1);
        anyhow::ensure!(
            after.delivered > before.delivered
                && after.admitted_udp_bytes > before.admitted_udp_bytes
                && directed_stats(low_edge.0, low_edge.1) == low_before
                && directed_stats(wan.0, wan.1) == wan_before
                && low_frames.get() - count >= 10,
            "endpoint impairment did not isolate the intended socket: {high_edge:?} {before:?} -> {after:?}"
        );
        remove_directed_link(high_edge.0, high_edge.1);
        expect_height(
            &high_sink,
            "isolated viewer recovery",
            720,
            Duration::from_secs(22),
            false,
        )
        .await?;
        anyhow::ensure!(
            udp.receive_edge(high_ssrc) == Some(high_edge)
                && udp.receive_edge(low_ssrc) == Some(low_edge),
            "VP9 receiver changed socket edges during the isolation witness"
        );
        Ok(())
    };
    tokio::select! {
        result = check_low => result,
        result = phases => result,
    }
}

async fn check_shared_hop_squeeze(
    sink: &VideoSink,
    (from, to): (SocketAddr, SocketAddr),
) -> anyhow::Result<Duration> {
    use pulsebeam_runtime::net::shaper::{
        Capacity, directed_stats, remove_directed_link, set_directed_capacity,
    };
    use pulsebeam_runtime::time::Instant;

    const SQUEEZE_BPS: u64 = 650_000;
    const BACKLOG: Duration = Duration::from_millis(100);
    const RAMP: Duration = Duration::from_secs(3);
    const STEADY: Duration = Duration::from_millis(1_200);
    let reverse = directed_stats(to, from);
    while sink.try_next_frame().is_some() {}
    set_directed_capacity(
        from,
        to,
        Capacity::Ramp {
            from: 3_000_000,
            to: SQUEEZE_BPS,
            over: RAMP,
        },
        BACKLOG,
    );
    let start = Instant::now();
    let mut last_frame = start;
    let counters = || {
        [
            "fabric_network_vp9_receive",
            "fabric_network_vp9_branch_offer",
            "fabric_rtp_vp9_wan_enqueued",
        ]
        .map(pulsebeam::sim_metrics::routing_counter)
    };
    let mut last_frame_counters = counters();
    let mut worst_gap = Duration::ZERO;
    let mut base = None;
    let mut heights = [0; 3];
    loop {
        let now = Instant::now();
        let stats = directed_stats(from, to);
        worst_gap = worst_gap.max(now - last_frame);
        if now - last_frame >= Duration::from_millis(450) {
            base = None;
        }
        while let Some(frame) = sink.try_next_frame() {
            let current = counters();
            let gap = now - last_frame;
            if gap >= Duration::from_millis(100) {
                let delta = std::array::from_fn::<_, 3, _>(|i| {
                    current[i].saturating_sub(last_frame_counters[i])
                });
                tracing::debug!(?gap, elapsed = ?(now - start), first_height = frame.height,
                    wan_receives = delta[0], branch_offers = delta[1], rtc_enqueued = delta[2],
                    ?stats, "VP9 decode resumed during WAN squeeze");
            }
            last_frame_counters = current;
            last_frame = now;
            let index = match frame.height {
                180 => 0,
                360 => 1,
                720 => 2,
                height => anyhow::bail!("unexpected decoded VP9 height {height}"),
            };
            heights[index] += 1;
            if index == 0 && now - start >= RAMP {
                base.get_or_insert((now, stats, heights[0]));
            } else {
                base = None;
            }
        }
        if let Some((since, before, first_base)) = base
            && now - since >= STEADY
        {
            let bytes = stats.admitted_udp_bytes - before.admitted_udp_bytes;
            let cap = u128::from(SQUEEZE_BPS) * (now - since + BACKLOG).as_millis() / 8_000;
            anyhow::ensure!(
                bytes > 0
                    && u128::from(bytes) <= cap
                    && heights[0] - first_base >= 10
                    && stats.dropped_overflow == before.dropped_overflow,
                "expected sustained decoded VP9 base without overflow at the directed wire-byte cap; bytes={bytes}/{cap}; heights={heights:?}; link={before:?} -> {stats:?}"
            );
            break;
        }
        anyhow::ensure!(
            now - start < Duration::from_secs(9),
            "no sustained decoded base under the WAN squeeze; heights={heights:?}; gap={worst_gap:?}; link={stats:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    remove_directed_link(from, to);
    anyhow::ensure!(
        directed_stats(to, from) == reverse,
        "WAN squeeze also shaped the reverse direction"
    );
    Ok(worst_gap)
}

pub(in crate::tests) async fn open_logical_h264(
    server: SocketAddr,
    host: NativeHost,
    subject: &str,
    publisher: &str,
    track: &str,
    publication: &str,
) -> anyhow::Result<(WirePeer, VideoSink)> {
    let (mut viewer, mut outputs) = connect_viewer(server, host, subject, &["h264"]).await?;
    let (binding, sink) = outputs.pop().context("missing H264 receive slot")?;
    viewer.desire(
        1,
        1,
        Some(wire::ResourceSpec {
            declaration: Some(wire::resource_spec::Declaration::LogicalSubscription(
                wire::LogicalSubscriptionSpec {
                    source: Some(wire::LogicalSourceAddress {
                        room: "stage".into(),
                        participant_id: publisher.into(),
                        kind: wire::TrackKind::Video as i32,
                        track_id: track.into(),
                    }),
                },
            )),
            settings: Some(wire::ResourceSettings {
                enabled: Some(true),
                receive: Some(wire::resource_settings::Receive::Video(
                    wire::VideoReceiveOptions::default(),
                )),
            }),
        }),
    );
    tokio::time::timeout(
        Duration::from_secs(8),
        bind_logical_video(&mut viewer, publication, &binding, 2, 0),
    )
    .await
    .context("logical H264 receiver did not install")??;
    Ok((viewer, sink))
}

async fn bind_logical_video(
    viewer: &mut WirePeer,
    publication: &str,
    binding: &wire::TrackBinding,
    report_revision: u64,
    previous_source: u64,
) -> anyhow::Result<u64> {
    use wire::logical_subscription_state::Execution;
    let offer = viewer
        .logical_when(1, |state| {
            state.source_revision > previous_source && state.source.is_some()
        })
        .await?;
    anyhow::ensure!(
        offer
            .source
            .as_ref()
            .is_some_and(|source| source.publication_id == publication),
        "logical video intent resolved to a different publication: {offer:?}"
    );
    viewer.report_prepared_receiver(offer.source_revision, report_revision, binding.clone());
    let settled = viewer
        .logical_when(1, |state| {
            state.source_revision >= offer.source_revision && state.execution.is_some()
        })
        .await?;
    anyhow::ensure!(
        settled.source_revision == offer.source_revision
            && settled.execution == Some(Execution::Installed(binding.clone())),
        "logical video receiver did not install: {settled:?}"
    );
    Ok(offer.source_revision)
}

async fn install(
    viewer: &mut WirePeer,
    reference: u64,
    revision: u64,
    spec: &wire::ResourceSpec,
) -> anyhow::Result<()> {
    viewer.desire(reference, revision, Some(spec.clone()));
    loop {
        let state = viewer.confirmed(reference, revision).await?;
        anyhow::ensure!(
            state.rejection.is_none(),
            "VP9 receive policy rejected: {state:?}"
        );
        let Some(wire::confirmed_state::Value::Owned(owned)) = state.value else {
            anyhow::bail!("VP9 subscription absent: {state:?}");
        };
        anyhow::ensure!(
            owned.execution != wire::ExecutionStatus::Failed as i32,
            "VP9 subscription failed: {owned:?}"
        );
        if owned.execution == wire::ExecutionStatus::Installed as i32 {
            return Ok(());
        }
    }
}
