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
    HeightChanges,
    Bandwidth {
        receiver_ip: IpAddr,
    },
}

pub(in crate::tests) async fn receive(
    address: SocketAddr,
    host: NativeHost,
    publication: &str,
    check: Check,
    start_replay: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut transceivers = Vec::new();
    // An exact RTP binding is single-use. Reserve a second negotiated receive
    // slot so logical recovery does not require reopening the PeerConnection.
    let slots = if matches!(check, Check::WanRepair { .. }) {
        2
    } else {
        1
    };
    let (mut viewer, _) = WirePeer::connect_with_peer(
        host,
        address,
        "vp9-viewer",
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
                    .find(|codec| codec.base_codec().name_bytes() == name)
                    .cloned()
                    .context("missing stock VP9/RTX capability")
            };
            let codecs = [codec(b"VP9")?, codec(b"rtx")?];
            for _ in 0..slots {
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
        .enumerate()
        .map(|(index, section)| {
            let codec = section
                .codecs
                .iter()
                .find(|codec| codec.parameters.name == "vp9")
                .context("negotiated VP9 codec")?;
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
        adopted.receptions.len() == slots,
        "missing negotiated VP9 receive slots"
    );
    let mut outputs = transceivers
        .iter()
        .zip(&adopted.receptions)
        .map(|(transceiver, binding)| {
            anyhow::ensure!(
                binding.complete()
                    && binding
                        .extensions
                        .iter()
                        .any(|extension| extension.uri == pulsebeam_core::dd::URI),
                "VP9 receive binding lacks DD: {binding:?}"
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
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter();
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
                        Check::HeightChanges
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
    let expect_height =
        async |sink: &VideoSink, label: &str, height: u32, wait: Duration, strict: bool| {
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
                        other => anyhow::bail!("unexpected VP9 frame height {other}"),
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
        };
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
        Check::HeightChanges => {
            let wait = Duration::from_secs(3);
            expect_height(&sink, "initial height ceiling", 180, wait, true).await?;
            for (revision, height) in [(2, 720), (3, 180), (4, 720)] {
                spec.settings = Some(wire::ResourceSettings {
                    enabled: Some(true),
                    receive: Some(wire::resource_settings::Receive::Video(
                        wire::VideoReceiveOptions {
                            max_height: Some(height),
                            ..Default::default()
                        },
                    )),
                });
                install(&mut viewer, reference, revision, &spec).await?;
                expect_height(&sink, "updated height ceiling", height, wait, false).await?;
            }
            Ok(())
        }
        Check::Bandwidth { receiver_ip } => {
            use pulsebeam_runtime::net::shaper::set_downlink;
            let wait = Duration::from_secs(9);
            set_downlink(receiver_ip, 10_000_000);
            expect_height(&sink, "unconstrained", 720, wait, false).await?;
            set_downlink(receiver_ip, 650_000);
            let before = pulsebeam_runtime::net::shaper::stats(receiver_ip);
            expect_height(&sink, "constrained", 180, wait, false)
                .await
                .with_context(|| {
                    let after = pulsebeam_runtime::net::shaper::stats(receiver_ip);
                    format!(
                        "VP9 downlink counters at {receiver_ip}: before={before:?}; after={after:?}"
                    )
                })?;
            anyhow::ensure!(
                pulsebeam_runtime::net::shaper::stats(receiver_ip).delivered > before.delivered,
                "RTC traffic did not traverse the constrained downlink"
            );
            set_downlink(receiver_ip, 10_000_000);
            // RTC probes upward from the constrained rate instead of jumping
            // straight to the full encoding cost when capacity returns.
            expect_height(
                &sink,
                "back to unconstrained",
                720,
                Duration::from_secs(22),
                false,
            )
            .await
        }
    }
}

async fn check_shared_hop_squeeze(
    sink: &VideoSink,
    (from, to): (SocketAddr, SocketAddr),
) -> anyhow::Result<Duration> {
    use pulsebeam_runtime::net::shaper::{
        Capacity, directed_stats, remove_directed_capacity, set_directed_capacity,
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
    remove_directed_capacity(from, to);
    anyhow::ensure!(
        directed_stats(to, from) == reverse,
        "WAN squeeze also shaped the reverse direction"
    );
    Ok(worst_gap)
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
        "logical VP9 intent resolved to a different publication: {offer:?}"
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
        "logical VP9 receiver did not install: {settled:?}"
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
