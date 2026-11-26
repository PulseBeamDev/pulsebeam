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
        } => wire::resource_spec::Declaration::LogicalSubscription(wire::LogicalSubscriptionSpec {
            source: Some(wire::LogicalSourceAddress {
                room: "stage".into(),
                participant_id: publisher.into(),
                kind: wire::TrackKind::Video as i32,
                track_id: track.into(),
            }),
            data_version: None,
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
                    max_height: matches!(check, Check::HeightChanges).then_some(180),
                    ..Default::default()
                },
            )),
        }),
    };
    let first_source = if matches!(check, Check::WanRepair { .. }) {
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
