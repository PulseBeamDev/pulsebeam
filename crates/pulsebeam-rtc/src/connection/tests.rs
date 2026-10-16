use super::*;
use crate::test_support::{
    NetworkPolicy, PeerFixture, RtpImpairment, assert_initial_probe_windows,
    assert_probe_windows, forwarded,
};
use crate::{
    ForwardedMedia, FrameBoundary, FrameDependencies, FrameId, FrameMetadata, TransmitTarget,
};
use std::{sync::Arc, time::Duration};

#[test]
fn potential_only_sender_obtains_padding_feedback_without_media() {
    let mut fixture = PeerFixture::connected().without_egress_observation();
    fixture.configure_rtc_wakeup_quantum(Duration::from_millis(1));
    fixture.configure_network(
        0x6602,
        NetworkPolicy {
            delay: Duration::from_millis(50),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_time_quantum(Duration::from_millis(5));
    fixture.command(Command::SetSenderDemand {
        sender: fixture.sender,
        demand: crate::SenderDemand::new(
            MediaPayloadBitrate::from_bps(0),
            MediaPayloadBitrate::from_bps(2_000_000),
        )
        .expect("potential-only demand"),
    });
    let received_before = fixture
        .connection
        .runtime
        .commit
        .history
        .counters()
        .received;
    let mut saw_fresh_delivery = false;
    let mut trace = Vec::new();
    for tick in 1..=160 {
        fixture.drive_for(Duration::from_millis(50));
        let envelope = fixture
            .connection
            ._subsystems
            .egress
            .test_envelope()
            .expect("controller envelope");
        saw_fresh_delivery |=
            !envelope.feedback_stale && envelope.delivered_rtp_transport_rate > 0;
        if tick % 20 == 0 {
            trace.push((tick / 20, envelope));
        }
    }
    let stats = fixture.connection.stats().connection;
    assert_eq!(
        stats.feedback,
        Some(crate::PacketFeedbackKind::TransportWide)
    );
    assert_eq!(stats.transmitted_rtp_bytes, 0);
    assert!(stats.transmitted_padding_bytes > 0);
    let initial_started = fixture
        .connection
        ._subsystems
        .egress
        .test_initial_probe_started_at()
        .expect("ordinary startup probe");
    assert_initial_probe_windows(fixture.emitted_rtp(), initial_started, 2_000_000);
    assert!(
        saw_fresh_delivery
            && fixture
                .connection
                .runtime
                .commit
                .history
                .counters()
                .received
                > received_before,
        "potential-only feedback: stats={stats:?} trace={trace:?}",
    );
    fixture.drive_for(Duration::from_secs(22));
    let bootstrap = fixture.connection.stats().connection;
    assert_eq!(bootstrap.transmitted_rtp_bytes, 0);
    assert!(
        bootstrap.target_media_bitrate.as_bps() >= 1_100_000,
        "potential-only demand must bootstrap native capacity within 30s: stats={bootstrap:?}",
    );
    assert_initial_probe_windows(fixture.emitted_rtp(), initial_started, 2_000_000);
}

#[test]
fn queued_sctp_output_precedes_potential_only_padding() {
    let mut fixture = PeerFixture::connected_datachannels();
    let mut channel = None;
    let mut peer_opened = false;
    while channel.is_none() || !peer_opened {
        match fixture.next_data_event() {
            crate::test_support::FixtureDataEvent::ConnectionOpened(id) => channel = Some(id),
            crate::test_support::FixtureDataEvent::PeerOpened => peer_opened = true,
            _ => {}
        }
    }
    fixture.command(Command::SetSenderDemand {
        sender: fixture.sender,
        demand: crate::SenderDemand::new(
            MediaPayloadBitrate::from_bps(0),
            MediaPayloadBitrate::from_bps(2_000_000),
        )
        .expect("potential-only interest"),
    });
    fixture.command(Command::SendData {
        channel: channel.expect("opened channel"),
        message: crate::DataMessage::Binary(bytes::Bytes::from(vec![7; 20_000])),
    });
    let before = fixture.connection.stats().connection;
    let at = fixture.at();
    let mut polls = 0;
    while fixture
        .connection
        ._subsystems
        .sctp
        .as_ref()
        .is_some_and(Association::has_packet)
    {
        assert!(polls < 128, "queued native output drains with bounded work");
        polls += 1;
        let _ = fixture.connection.poll(at);
        assert_eq!(
            fixture
                .connection
                .stats()
                .connection
                .transmitted_padding_bytes,
            before.transmitted_padding_bytes,
        );
    }
    assert!(polls > 0);
    assert!(
        fixture.connection.stats().connection.transmitted_sctp_bytes
            > before.transmitted_sctp_bytes
    );
    assert!(
        fixture
            .connection
            ._subsystems
            .egress
            .test_envelope()
            .expect("controller envelope")
            .probe_permitted
    );
}

#[test]
fn queued_media_and_repair_prevent_padding_without_blocking_real_rtp() {
    for repair in [false, true] {
        let mut fixture = if repair {
            PeerFixture::connected_video()
        } else {
            PeerFixture::connected()
        };
        if repair {
            fixture.command(Command::SetSenderPolicy {
                sender: fixture.sender,
                policy: crate::SenderPolicy {
                    desired_bitrate: MediaPayloadBitrate::from_bps(1_000_000),
                    priority: crate::MediaPriority::MEDIUM,
                    playout_delay: None,
                },
            });
        }
        let packet = fixture.send_source(if repair { &[0x65; 32] } else { &[7; 32] });
        let at = fixture.at();
        fixture.connection._subsystems.egress.update_controller(
            at,
            Some((1, true)),
            &[],
            &[],
            Duration::ZERO,
            false,
            0,
        );
        fixture
            .try_command(Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(packet, 0),
            })
            .expect("admit queued priority fixture");
        if repair {
            let header = fixture.receive_video_wire_header();
            fixture
                .connection
                ._subsystems
                .egress
                .request_repair(header.ssrc, header.sequence_number);
        }
        fixture.command(Command::SetSenderDemand {
            sender: fixture.sender,
            demand: crate::SenderDemand::new(
                MediaPayloadBitrate::from_bps(0),
                MediaPayloadBitrate::from_bps(2_000_000),
            )
            .expect("potential-only interest"),
        });
        let at = fixture.at();
        let sender = fixture.sender;
        let subsystems = &mut fixture.connection._subsystems;
        subsystems.egress.update_controller(
            at,
            Some((1, true)),
            &[],
            &[],
            Duration::ZERO,
            false,
            0,
        );
        assert!(subsystems.egress.test_envelope().unwrap().probe_permitted);
        let (_, senders) = subsystems.egress.stats();
        assert_eq!(senders[0].allocation.as_bps(), 0);
        assert_eq!(senders[0].queued_packets, usize::from(!repair));
        assert!(matches!(
            subsystems
                .egress
                .test_prepare_padding(at, 0, &mut subsystems.transport),
            PrepareResult::Blocked
        ));
        let mut service = None;
        for _ in 0..2 {
            assert!(matches!(
                subsystems
                    .egress
                    .prepare_one(at, 0, &mut subsystems.transport, true),
                PrepareResult::Prepared
            ));
            let transmit = subsystems.transport.poll_prepared().unwrap();
            if let Some(rtp) = transmit.rtp {
                service = Some(rtp.service);
            } else {
                assert_eq!(transmit.kind, DatagramKind::Rtcp);
            }
            subsystems.egress.commit(at, &transmit);
            if service.is_some() {
                break;
            }
        }
        assert_eq!(
            service,
            Some(if repair {
                crate::transport::RtpService::Repair
            } else {
                crate::transport::RtpService::Original
            })
        );
        subsystems.egress.discard_sender_media(sender).unwrap();
        let idle_at = TimePoint {
            monotonic: at.monotonic + Duration::from_millis(20),
            global: at.global.checked_add(Duration::from_millis(20)).unwrap(),
        };
        assert!(matches!(
            subsystems
                .egress
                .prepare_one(idle_at, 0, &mut subsystems.transport, true),
            PrepareResult::Prepared
        ));
    }
}

#[test]
#[should_panic(expected = "emitted probe budget")]
fn probe_oracle_rejects_credit_from_later_emission_at_same_instant() {
    let now = Instant::now();
    assert_probe_windows(&[(now, 48 * 1024 + 1, true), (now, 19, false)]);
}

#[test]
#[should_panic(expected = "emitted probe budget")]
fn probe_oracle_rejects_overspend_when_media_credit_expires() {
    let now = Instant::now();
    assert_probe_windows(&[
        (now, 19_000, false),
        (now + Duration::from_secs(1), 48 * 1024 + 1_000, true),
    ]);
}

#[test]
#[should_panic(expected = "emitted probe budget")]
fn initial_probe_oracle_rejects_more_than_demand_sized_service() {
    let now = Instant::now();
    assert_initial_probe_windows(&[(now, 60_001, true)], now, 1_200_000);
}

#[test]
#[should_panic(expected = "emitted probe budget")]
fn initial_probe_oracle_rejects_refreshed_credit_after_initial_period() {
    let now = Instant::now();
    assert_initial_probe_windows(
        &[(now + Duration::from_secs(5), 48 * 1024 + 1, true)],
        now,
        2_000_000,
    );
}

#[test]
fn initial_probe_oracle_retains_spent_service_without_granting_new_credit() {
    let now = Instant::now();
    assert_initial_probe_windows(
        &[(now + Duration::from_secs(4), 96 * 1024, true)],
        now,
        2_000_000,
    );
}

#[test]
fn production_feedback_outage_path_replacement_and_media_pause_recover() {
    let mut fixture = PeerFixture::connected().without_egress_observation();
    fixture.configure_network(
        0x6601,
        NetworkPolicy {
            delay: Duration::from_millis(25),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_bottleneck(2_000_000);
    fixture.configure_time_quantum(Duration::from_millis(5));
    let mut source = crate::test_source::ComponentSource::audio(1);
    let mut policy = crate::ConnectionConfig::default().default_audio_policy;
    policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 50).expect("500 ms"));
    policy.desired_bitrate = MediaPayloadBitrate::from_bps(2_000_000);
    fixture.command(Command::SetSenderPolicy {
        sender: fixture.sender,
        policy,
    });
    let mut frame = 0;
    for (feedback_enabled, replace_path) in
        [(true, false), (false, false), (true, false), (true, true)]
    {
        fixture.set_feedback_enabled(feedback_enabled);
        if replace_path {
            assert_probe_windows(fixture.emitted_rtp());
            for _ in 0..2 {
                fixture
                    .connection
                    ._subsystems
                    .transport
                    .reselect_path_for_test()
                    .expect("replace selected path");
            }
            fixture.command(Command::SetSenderPolicy {
                sender: fixture.sender,
                policy,
            });
        }
        let start = fixture.at().monotonic;
        let before = fixture.connection.stats().connection.transmitted_rtp_bytes;
        for tick in 1..=600 {
            let at = start + Duration::from_millis(tick * 5);
            fixture.drive_for(at.saturating_duration_since(fixture.at().monotonic));
            frame += 1;
            let media = forwarded(source.sample(fixture.at(), &[0x5a; 1_000]), frame);
            match fixture.try_command(Command::SendMedia {
                sender: fixture.sender,
                media,
            }) {
                Ok(()) | Err(CommandError::WouldBlock) => {}
                Err(error) => panic!("unexpected media error: {error:?}"),
            }
        }
        let envelope = fixture
            .connection
            ._subsystems
            .egress
            .test_envelope()
            .expect("controller output");
        assert_eq!(envelope.feedback_stale, !feedback_enabled);
        assert!(fixture.connection.stats().connection.transmitted_rtp_bytes > before);
    }
    fixture.drive_for(Duration::from_secs(1));
    let paused = fixture.connection.stats().connection.transmitted_rtp_bytes;
    fixture.drive_for(Duration::from_secs(2));
    assert_eq!(
        fixture.connection.stats().connection.transmitted_rtp_bytes,
        paused
    );
    assert!(
        fixture
            .connection
            ._subsystems
            .egress
            .test_envelope()
            .expect("controller output")
            .application_limited
    );
    let start = fixture.at().monotonic;
    for tick in 1..=600 {
        let at = start + Duration::from_millis(tick * 5);
        fixture.drive_for(at.saturating_duration_since(fixture.at().monotonic));
        frame += 1;
        let media = forwarded(source.sample(fixture.at(), &[0x5a; 1_000]), frame);
        match fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media,
        }) {
            Ok(()) | Err(CommandError::WouldBlock) => {}
            Err(error) => panic!("unexpected media error: {error:?}"),
        }
    }
    assert!(fixture.connection.stats().connection.transmitted_rtp_bytes > paused);
    assert!(
        !fixture
            .connection
            ._subsystems
            .egress
            .test_envelope()
            .expect("controller output")
            .feedback_stale
    );
    assert_probe_windows(fixture.emitted_rtp());
    fixture.drive_for(Duration::from_secs(1));
    let stopped = fixture.connection.stats().connection.transmitted_rtp_bytes;
    fixture.drive_for(Duration::from_secs(6));
    assert_eq!(
        fixture.connection.stats().connection.transmitted_rtp_bytes,
        stopped
    );
    assert_probe_windows(fixture.emitted_rtp());
}

#[test]
fn production_random_burst_policer_and_heterogeneous_traces() {
    for (name, impairment) in [
        (
            "random-1-percent",
            Some(RtpImpairment::Random { per_mille: 10 }),
        ),
        (
            "burst-6-of-200",
            Some(RtpImpairment::Burst {
                every: 200,
                length: 6,
            }),
        ),
        (
            "policer-125kbps-3000bytes",
            Some(RtpImpairment::Policer {
                bits_per_second: 125_000,
                burst_bytes: 3_000,
            }),
        ),
        ("cubic-like", None),
        ("bbr-like", None),
    ] {
        let mut fixture = PeerFixture::connected().without_egress_observation();
        fixture.configure_network(
            0x7701,
            NetworkPolicy {
                delay: Duration::from_millis(25),
                ..NetworkPolicy::default()
            },
        );
        fixture.configure_bottleneck(4_000_000);
        fixture.configure_time_quantum(Duration::from_millis(5));
        fixture.observe_native_target(|connection| {
            connection
                ._subsystems
                .egress
                .test_envelope()
                .expect("envelope")
                .native_queue_delay_target
        });
        let delivered_before_observation = fixture.delivered_rtp().len();
        if let Some(impairment) = impairment {
            fixture.configure_impairment(impairment);
        }
        let mut source = crate::test_source::ComponentSource::audio(1);
        let mut policy = crate::ConnectionConfig::default().default_audio_policy;
        policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 50).expect("500 ms"));
        policy.desired_bitrate = MediaPayloadBitrate::from_bps(4_000_000);
        fixture.command(Command::SetSenderPolicy {
            sender: fixture.sender,
            policy,
        });
        let start = fixture.at().monotonic;
        let mut policed = false;
        let mut cross_bytes = 0;
        for tick in 1..=3_000_u64 {
            let at = start + Duration::from_millis(tick * 5);
            fixture.drive_for(at.saturating_duration_since(fixture.at().monotonic));
            // Open-loop pacing traces, not TCP implementations or equal-share promises.
            let cross_rate = match name {
                "cubic-like" => {
                    let phase = i64::try_from((tick * 5) % 4_000).expect("phase") - 2_000;
                    u64::try_from(750_000 + phase.pow(3) / 16_000)
                        .expect("positive cubic trace")
                }
                "bbr-like" => match (tick / 10) % 8 {
                    0 => 1_250_000,
                    1 => 750_000,
                    _ => 1_000_000,
                },
                _ => 0,
            };
            if cross_rate > 0 {
                let bytes = cross_rate * 5 / 8_000;
                fixture.inject_cross_traffic(bytes);
                cross_bytes += bytes;
            }
            for packet in 0..2 {
                let packet_at = at + Duration::from_micros(packet * 2_500);
                fixture.drive_for(packet_at.saturating_duration_since(fixture.at().monotonic));
                let media = forwarded(
                    source.sample(fixture.at(), &[0x5a; 1_000]),
                    tick * 2 + packet,
                );
                match fixture.try_command(Command::SendMedia {
                    sender: fixture.sender,
                    media,
                }) {
                    Ok(()) | Err(CommandError::WouldBlock) => {}
                    Err(error) => panic!("{name} admission: {error:?}"),
                }
            }
            let stats = fixture.connection.stats();
            assert!(stats.connection.queued_media_bytes <= 8 * 1024 * 1024);
            assert!(stats.connection.rtp_bytes_in_flight <= 8 * 1024 * 1024);
            let envelope = fixture
                .connection
                ._subsystems
                .egress
                .test_envelope()
                .expect("envelope");
            policed |= envelope.policer_detected;
            assert!(
                !envelope.feedback_stale,
                "{name}: covering feedback remains live"
            );
        }
        fixture.drive_for(Duration::from_secs(2));
        let delivered: u64 = fixture
            .delivered_rtp()
            .iter()
            .filter(|(at, _)| {
                *at >= start + Duration::from_secs(10) && *at < start + Duration::from_secs(15)
            })
            .map(|(_, bytes)| *bytes)
            .sum();
        let (sent, dropped) = fixture.impairment_counts();
        assert!(
            delivered > 10_000,
            "{name}: sustained late delivery, not startup-only success"
        );
        if impairment.is_some() {
            assert!(dropped > 0 && dropped < sent);
        }
        if matches!(impairment, Some(RtpImpairment::Policer { .. })) {
            assert!(policed, "production policer detection must be exercised");
        }
        let mut queue = fixture
            .rtp_queue_samples()
            .iter()
            .map(|(_, delay, _)| *delay)
            .collect::<Vec<_>>();
        queue.sort_unstable();
        assert_eq!(
            queue.len(),
            fixture.delivered_rtp().len() - delivered_before_observation,
            "queue samples must include every delivered RTP in the observation window"
        );
        assert_probe_windows(fixture.emitted_rtp());
        eprintln!(
            "production profile={name} seed=0x7701 link=4Mbps RTT=50ms quantum=5ms offered=3.2Mbps desired=4Mbps duration=15s sent={sent} drops={dropped} late_delivered={delivered} cross_bytes={cross_bytes} policed={policed} queue_p50={:?} queue_p95={:?} queue_p99={:?}",
            queue[(queue.len() * 50).div_ceil(100) - 1],
            queue[(queue.len() * 95).div_ceil(100) - 1],
            queue[(queue.len() * 99).div_ceil(100) - 1]
        );
    }
}

#[test]
fn production_demand_limited_video_vbr_keyframes_keep_queue_bound() {
    let mut fixture = PeerFixture::connected_video().without_egress_observation();
    fixture.configure_network(
        0x7702,
        NetworkPolicy {
            delay: Duration::from_millis(25),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_bottleneck(2_000_000);
    fixture.configure_time_quantum(Duration::from_millis(5));
    fixture.observe_native_target(|connection| {
        connection
            ._subsystems
            .egress
            .test_envelope()
            .expect("envelope")
            .native_queue_delay_target
    });
    let mut source = crate::test_source::ComponentSource::video(1);
    let mut policy = crate::ConnectionConfig::default().default_video_policy;
    policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 50).expect("500 ms"));
    policy.desired_bitrate = MediaPayloadBitrate::from_bps(2_000_000);
    fixture.command(Command::SetSenderPolicy {
        sender: fixture.sender,
        policy,
    });
    let start = fixture.at().monotonic;
    for frame in 1..=1_600_u64 {
        let at = start + Duration::from_millis(frame * 25);
        fixture.drive_for(at.saturating_duration_since(fixture.at().monotonic));
        let key = frame % 40 == 1;
        let size = if key {
            1_000
        } else if frame % 2 == 0 {
            200
        } else {
            600
        };
        let mut payload = vec![0x5a; size];
        // Synthetic H.264 slices: IDR for keyframes, non-IDR for dependents.
        payload[0] = if key { 0x65 } else { 0x41 };
        let mut media = forwarded(source.sample(fixture.at(), &payload), frame);
        media.frame.random_access = key;
        media.frame.dependencies = FrameDependencies::known(if key {
            vec![]
        } else {
            vec![FrameId::from_value(frame - 1)]
        })
        .expect("frame dependencies");
        fixture.command(Command::SendMedia {
            sender: fixture.sender,
            media,
        });
    }
    fixture.drive_for(Duration::from_secs(2));
    // Demand-limited 40fps VBR: fixed 2s warmup and 36s interval, including keyframes.
    let samples = fixture
        .rtp_queue_samples()
        .iter()
        .filter(|(at, _, _)| {
            *at >= start + Duration::from_secs(2) && *at < start + Duration::from_secs(38)
        })
        .collect::<Vec<_>>();
    let mut raw = samples
        .iter()
        .map(|(_, delay, _)| *delay)
        .collect::<Vec<_>>();
    let mut excess = samples
        .iter()
        .map(|(_, delay, target)| delay.saturating_sub(*target))
        .collect::<Vec<_>>();
    raw.sort_unstable();
    excess.sort_unstable();
    assert!(samples.len() >= 1_000);
    assert_eq!(fixture.network_counters().1, 0);
    assert!(excess[(excess.len() * 99).div_ceil(100) - 1] <= Duration::from_millis(10));
    assert_probe_windows(fixture.emitted_rtp());
    eprintln!(
        "production demand-limited VBR video seed=0x7702 offered_payload=128kbps link=2Mbps RTT=50ms quantum=5ms frames=1600 keyframe_every=40 sizes=1000/200/600B samples={} drops=0 raw_p50={:?} raw_p95={:?} raw_p99={:?}",
        samples.len(),
        raw[(raw.len() * 50).div_ceil(100) - 1],
        raw[(raw.len() * 95).div_ceil(100) - 1],
        raw[(raw.len() * 99).div_ceil(100) - 1]
    );
}

#[test]
fn production_queue_sojourn_tracks_contemporaneous_native_target() {
    let mut fixture = PeerFixture::connected().without_egress_observation();
    fixture.configure_network(
        0x5501,
        NetworkPolicy {
            delay: Duration::from_millis(25),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_bottleneck(2_000_000);
    fixture.configure_time_quantum(Duration::from_millis(2));
    fixture.observe_native_target(|connection| {
        connection._subsystems.egress.native_queue_delay_target()
    });
    let mut policy = crate::ConnectionConfig::default().default_audio_policy;
    policy.desired_bitrate = crate::MediaPayloadBitrate::from_bps(4_000_000);
    policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 50).expect("500 ms"));
    fixture.command(Command::SetSenderPolicy {
        sender: fixture.sender,
        policy,
    });
    let start = fixture.at().monotonic;
    let mut source = crate::test_source::ComponentSource::audio(1);
    for id in 1..=6_000_u64 {
        let tick = start + Duration::from_millis(id * 2);
        fixture.drive_for(tick.saturating_duration_since(fixture.at().monotonic));
        let media = forwarded(source.sample(fixture.at(), &[0x5a; 1_000]), id);
        match fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media,
        }) {
            Ok(()) | Err(CommandError::WouldBlock) => {}
            Err(error) => panic!("unexpected media error: {error:?}"),
        }
    }
    fixture.command(Command::SetSenderDemand {
        sender: fixture.sender,
        demand: crate::SenderDemand::new(
            MediaPayloadBitrate::from_bps(0),
            MediaPayloadBitrate::from_bps(0),
        )
        .expect("source stopped before draining committed packets"),
    });
    fixture.drive_for(Duration::from_secs(2));
    assert_eq!(fixture.network_counters().1, 0);
    assert_eq!(
        fixture
            .delivered_rtp()
            .iter()
            .map(|(_, bytes)| *bytes)
            .sum::<u64>(),
        fixture
            .connection
            .stats()
            .connection
            .transmitted_rtp_bytes
            .saturating_add(
                fixture
                    .connection
                    .stats()
                    .connection
                    .transmitted_padding_bytes
            ),
        "only delivered RTP may enter queue percentiles"
    );
    let stable_start = start + Duration::from_secs(2);
    let stable_end = stable_start + Duration::from_secs(10);
    let eligible = |at: &Instant| {
        *at >= stable_start
            && *at < stable_end
            && !fixture.emitted_rtp().iter().any(|(probe_at, _, probe)| {
                *probe
                    && *at >= *probe_at
                    && at.saturating_duration_since(*probe_at) < Duration::from_millis(120)
            })
    };
    let mut raw = fixture
        .rtp_queue_samples()
        .iter()
        .filter(|(at, _, _)| eligible(at))
        .map(|(_, sojourn, _)| *sojourn)
        .collect::<Vec<_>>();
    raw.sort_unstable();
    let mut excess = fixture
        .rtp_queue_samples()
        .iter()
        .filter(|(at, _, _)| eligible(at))
        .map(|(_, sojourn, target)| sojourn.saturating_sub(*target))
        .collect::<Vec<_>>();
    excess.sort_unstable();
    assert!(excess.len() >= 1_000);
    let p99 = excess[(excess.len() * 99).div_ceil(100) - 1];
    eprintln!(
        "production queue seed=0x5501 samples={} raw_p50={:?} raw_p95={:?} raw_p99={:?} excess_p99={p99:?}",
        excess.len(),
        raw[(raw.len() * 50).div_ceil(100) - 1],
        raw[(raw.len() * 95).div_ceil(100) - 1],
        raw[(raw.len() * 99).div_ceil(100) - 1],
    );
    assert!(p99 <= Duration::from_millis(10));
}

#[test]
fn tightening_policy_drops_queued_dependents_but_preserves_independent_frame() {
    let mut fixture = PeerFixture::connected();
    fixture.drive_for(Duration::from_millis(200));
    let mut source = PeerFixture::connected();
    let bytes = source.send_source(&[0x5a; 100]).bytes().clone();
    let at = fixture.at();
    let mut policy = crate::ConnectionConfig::default().default_audio_policy;
    policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 50).expect("500 ms"));
    policy.desired_bitrate = crate::MediaPayloadBitrate::from_bps(2_000_000);
    fixture.command(Command::SetSenderPolicy {
        sender: fixture.sender,
        policy,
    });
    for (id, offset, dependencies, random_access) in [
        (1, 0, vec![], true),
        (2, 100, vec![FrameId::from_value(1)], false),
        (3, 200, vec![], true),
    ] {
        let packet = crate::MediaPacket::new(
            bytes.clone(),
            at.global
                .checked_add(Duration::from_millis(offset))
                .expect("media global time"),
            Arc::from([]),
        );
        fixture
            .connection
            .command(
                at,
                Command::SendMedia {
                    sender: fixture.sender,
                    media: ForwardedMedia {
                        packet,
                        frame: FrameMetadata {
                            decode_target_mask: None,
                            id: FrameId::from_value(id),
                            boundary: FrameBoundary::Complete,
                            random_access,
                            discardable: false,
                            dependencies: FrameDependencies::known(dependencies)
                                .expect("dependencies"),
                        },
                    },
                },
            )
            .unwrap_or_else(|error| panic!("frame {id}: {error:?}"));
    }
    assert_eq!(fixture.connection.stats().senders[0].queued_packets, 3);
    policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 10).expect("100 ms"));
    fixture
        .connection
        .command(
            TimePoint {
                monotonic: at.monotonic + Duration::from_millis(50),
                global: at.global.checked_add(Duration::from_millis(50)).unwrap(),
            },
            Command::SetSenderPolicy {
                sender: fixture.sender,
                policy,
            },
        )
        .expect("tighten policy");
    assert_eq!(fixture.connection.stats().senders[0].queued_packets, 1);
}

#[test]
fn low_rate_media_uses_actual_offer_for_application_limited_classification() {
    let mut fixture = PeerFixture::connected();
    let mut source = PeerFixture::connected();
    let mut policy = crate::ConnectionConfig::default().default_audio_policy;
    policy.desired_bitrate = crate::MediaPayloadBitrate::from_bps(4_000_000);
    policy.playout_delay = Some(crate::PlayoutDelay::from_ticks(0, 50).expect("500 ms"));
    fixture.command(Command::SetSenderPolicy {
        sender: fixture.sender,
        policy,
    });
    for id in 1..=10 {
        source.drive_for(Duration::from_millis(100));
        fixture.drive_for(Duration::from_millis(100));
        let media = forwarded(source.send_source(&[0x5a; 1_000]), id);
        fixture.command(Command::SendMedia {
            sender: fixture.sender,
            media,
        });
    }
    fixture.drive_for(Duration::from_millis(300));
    let (offered, application_limited) = fixture.connection._subsystems.egress.observed_offer();
    assert!(offered <= 100_000, "actual sparse offer={offered}");
    assert!(application_limited);
    assert!(fixture.connection.stats().connection.transmitted_rtp_bytes > 0);
}

#[test]
fn failed_commit_preserves_accounting_and_valid_commit_preserves_wire_identity() {
    use crate::rtcp::{FeedbackBatch, FeedbackReport, TwccStatus};

    let monotonic = Instant::now();
    let at = TimePoint {
        monotonic,
        global: crate::GlobalMediaTime::from_micros(1),
    };
    let epoch = PathEpoch::from_value(3);
    let target = TransmitTarget::IceTcp {
        flow: crate::IceTcpFlowId::from_value(11),
    };
    let prepared = |path_epoch| PreparedTransmit {
        target: target.clone(),
        bytes: vec![0, 3, 1, 2, 3],
        kind: DatagramKind::Rtp,
        wire_len: 5,
        path_epoch: Some(path_epoch),
        rtp: Some(PreparedRtpIdentity {
            ssrc: 7,
            sequence: 9,
            twcc_sequence: Some(13),
            service: RtpService::Original,
        }),
    };
    let mut coordinator = CommitCoordinator::new(PacketFeedbackKind::TransportWide);
    coordinator.history.path_changed(epoch, true);
    let before = coordinator.history.counters();

    assert_eq!(
        coordinator.commit_transport(at, prepared(PathEpoch::from_value(4)), None),
        Err(HistoryError::InvalidCommit)
    );
    assert_eq!(coordinator.history.counters(), before);
    assert_eq!(coordinator.history.controller_inputs().bytes_in_flight, 0);
    assert_eq!(coordinator.counters, CommitCounters::default());

    let transmit = coordinator
        .commit_transport(at, prepared(epoch), None)
        .expect("valid commit after a rejected path");
    assert_eq!(transmit.target, target);
    assert_eq!(transmit.payload, Bytes::from_static(&[0, 3, 1, 2, 3]));
    assert_eq!(coordinator.counters.rtp_bytes, 5);
    assert_eq!(coordinator.history.controller_inputs().bytes_in_flight, 5);

    coordinator.history.process_feedback(FeedbackBatch {
        received_at: TimePoint {
            monotonic: monotonic + Duration::from_millis(20),
            global: at.global.checked_add(Duration::from_millis(20)).unwrap(),
        },
        path_epoch: epoch,
        sender_ssrc: 9,
        report: FeedbackReport::Twcc {
            media_ssrc: 7,
            base_sequence: 13,
            reference_time: 0,
            feedback_count: 1,
            statuses: vec![TwccStatus::Received { delta_250us: 1 }].into(),
        },
    });
    let inputs = coordinator.history.controller_inputs();
    assert_eq!(inputs.bytes_in_flight, 0);
    assert_eq!(inputs.feedback.len(), 1);
    let feedback = &inputs.feedback[0];
    assert_eq!(feedback.ssrc, 7);
    assert_eq!(feedback.committed_at, monotonic);
    assert_eq!(feedback.transport_bytes, 5);
    assert_eq!(feedback.service, RtpService::Original);
    assert!(feedback.received);
    assert_eq!(coordinator.history.counters().received, 1);
}

#[test]
fn poll_observes_committed_flight_before_batched_ack_retirement() {
    use crate::rtcp::{FeedbackBatch, FeedbackReport, TwccStatus};
    use crate::transport::{PreparedRtpIdentity, RtpService};

    let mut fixture = PeerFixture::connected();
    fixture.set_feedback_enabled(false);
    fixture.command(Command::SetSenderDemand {
        sender: fixture.sender,
        demand: crate::SenderDemand::new(
            MediaPayloadBitrate::from_bps(0),
            MediaPayloadBitrate::from_bps(0),
        )
        .expect("no live media demand"),
    });
    let epoch = fixture
        .connection
        .runtime
        .commit
        .history
        .test_active_epoch()
        .expect("connected path");
    fixture.connection.runtime.commit.history =
        SentHistory::new(crate::PacketFeedbackKind::TransportWide);
    fixture
        .connection
        .runtime
        .commit
        .history
        .path_changed(epoch, true);
    let _ = fixture.connection.poll(fixture.at());
    for sequence in 1..=16 {
        let context = TransmitCommitContext {
            at: fixture.at().monotonic,
            kind: DatagramKind::Rtp,
            wire_len: 1_200,
            path_epoch: Some(epoch),
            rtp: Some(PreparedRtpIdentity {
                ssrc: 7,
                sequence,
                twcc_sequence: Some(sequence),
                service: RtpService::Padding,
            }),
        };
        fixture
            .connection
            .runtime
            .commit
            .history
            .preflight_commit(context)
            .expect("valid commit");
        fixture
            .connection
            .runtime
            .commit
            .history
            .commit_preflighted(context);
    }
    assert_eq!(
        fixture
            .connection
            .runtime
            .commit
            .history
            .controller_inputs()
            .bytes_in_flight,
        19_200
    );
    assert!(fixture.connection._subsystems.egress.test_flight_peak() < 19_200);
    fixture.advance_clock_without_poll(Duration::from_millis(50));
    fixture
        .connection
        ._subsystems
        .ingress
        .test_queue_feedback(FeedbackBatch {
            received_at: fixture.at(),
            path_epoch: epoch,
            sender_ssrc: 9,
            report: FeedbackReport::Twcc {
                media_ssrc: 7,
                base_sequence: 1,
                reference_time: 0,
                feedback_count: 1,
                statuses: vec![TwccStatus::Received { delta_250us: 1 }; 16].into(),
            },
        });
    for _ in 0..32 {
        let output = fixture.connection.poll(fixture.at());
        assert!(
            !matches!(output, Output::Closed(_)),
            "unexpected closure: {output:?}"
        );
        if fixture
            .connection
            .runtime
            .commit
            .history
            .counters()
            .received
            == 16
        {
            break;
        }
    }
    assert_eq!(
        fixture
            .connection
            .runtime
            .commit
            .history
            .counters()
            .received,
        16,
        "queued ACK must retire the committed batch: {:?}",
        fixture.connection.runtime.commit.history.counters(),
    );
    assert!(
        fixture
            .connection
            .runtime
            .commit
            .history
            .controller_inputs()
            .bytes_in_flight
            < 19_200
    );
    assert_eq!(
        fixture.connection._subsystems.egress.test_flight_peak(),
        19_200
    );
}

#[test]
fn authenticated_twcc_feedback_drains_through_the_public_runtime() {
    let mut fixture = crate::test_support::PeerFixture::connected();
    let packet = fixture.send_source(b"feedback");
    for id in 1..=2 {
        fixture
            .connection
            .command(
                fixture.at(),
                Command::SendMedia {
                    sender: fixture.sender,
                    media: ForwardedMedia {
                        packet: packet.clone(),
                        frame: FrameMetadata {
                            decode_target_mask: None,
                            id: FrameId::from_value(id),
                            boundary: FrameBoundary::Complete,
                            random_access: true,
                            discardable: false,
                            dependencies: FrameDependencies::Known(Arc::from([])),
                        },
                    },
                },
            )
            .expect("media admitted through public command");
        let (header, payload) = fixture.receive_egress();
        assert_eq!(payload, b"feedback");
        assert!(header.ext_vals.transport_cc.is_some());
    }
    fixture.drive_for(std::time::Duration::from_secs(2));

    let counters = fixture.connection.runtime.commit.history.counters();
    assert!(
        counters.received > 0,
        "authenticated peer TWCC must acknowledge the public RTP commit: {counters:?}, peer generated {}",
        fixture.twcc_sent()
    );
    assert!(
        fixture
            .connection
            ._subsystems
            .ingress
            .poll_feedback()
            .is_none()
    );
}

#[test]
fn data_channel_commits_are_excluded_from_rtp_history_and_bytes_in_flight() {
    use crate::test_support::FixtureDataEvent;

    let mut fixture = crate::test_support::PeerFixture::connected_datachannels();
    let mut connection_channel = None;
    let mut peer_opened = false;
    while connection_channel.is_none() || !peer_opened {
        match fixture.next_data_event() {
            FixtureDataEvent::ConnectionOpened(channel) => connection_channel = Some(channel),
            FixtureDataEvent::PeerOpened => peer_opened = true,
            _ => {}
        }
    }
    let before_counters = fixture.connection.runtime.commit.history.counters();
    let before_bif = fixture
        .connection
        .runtime
        .commit
        .history
        .controller_inputs()
        .bytes_in_flight;
    let before_sctp = fixture
        .connection
        ._subsystems
        .sctp
        .as_ref()
        .expect("negotiated SCTP")
        .stats()
        .committed_transport_bytes;

    fixture.command(Command::SendData {
        channel: connection_channel.expect("connection channel"),
        message: crate::DataMessage::Binary(Bytes::from_static(b"separate accounting")),
    });
    while !matches!(
        fixture.next_data_event(),
        FixtureDataEvent::PeerMessage { .. }
    ) {}

    assert_eq!(
        fixture.connection.runtime.commit.history.counters(),
        before_counters
    );
    assert_eq!(
        fixture
            .connection
            .runtime
            .commit
            .history
            .controller_inputs()
            .bytes_in_flight,
        before_bif
    );
    assert!(
        fixture
            .connection
            ._subsystems
            .sctp
            .as_ref()
            .expect("negotiated SCTP")
            .stats()
            .committed_transport_bytes
            > before_sctp
    );
}
