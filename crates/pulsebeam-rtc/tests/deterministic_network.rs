#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    reason = "the deterministic system harness prints its seed/trace and fails at the first invariant"
)]

mod support;

use std::time::Duration;

use bytes::Bytes;
use pulsebeam_rtc::{CloseReason, NetworkInput, Output, WriteError};
use support::{NetworkPolicy, PeerFixture, rtp_write};

const PAYLOAD_BYTES: usize = 1_000;
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(50);

#[test]
fn rate_controlled_bursts_resume_after_feedback_outage_and_uncommitted_discard() {
    use pulsebeam_rtc::{MediaPayloadBitrate, PacketFeedbackKind};

    for mode in [
        PacketFeedbackKind::TransportWide,
        PacketFeedbackKind::Rfc8888,
    ] {
        let mut fixture = PeerFixture::connected_with_feedback(mode);
        fixture.configure_network(
            0x2910,
            NetworkPolicy {
                delay: Duration::from_millis(100),
                drop_every: None,
                duplicate_every: Some(5),
                reorder_every: None,
            },
        );
        fixture.configure_bottleneck(600_000);
        let mut sequence = 1;
        for (phase, payload_bytes, demand) in
            [(0, 400, 160_000), (1, 600, 240_000), (2, 400, 160_000)]
        {
            fixture.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(demand)));
            let before = fixture.connection.stats();
            for frame in 0..10 {
                for packet in 0..2 {
                    let mut write = support::sample(fixture.at(), sequence, payload_bytes, 48_000);
                    write.timestamp = (phase * 10 + frame) * 1_920;
                    write.marker = packet == 1;
                    fixture
                        .write_rtp(write)
                        .expect("bounded encoder burst admission");
                    sequence += 1;
                }
                fixture.drive_for(Duration::from_millis(40));
            }
            if phase == 1 {
                let committed = fixture.connection.stats();
                assert!(
                    committed.connection.rtp_bytes_in_flight > 0,
                    "feedback outage must exercise outstanding flight: {mode:?}"
                );
                for _ in 0..3 {
                    fixture
                        .write_rtp(support::sample(fixture.at(), sequence, 600, 48_000))
                        .expect("discardable admission");
                    sequence += 1;
                }
                let queued = fixture.connection.stats();
                let mid = fixture.sender_mid.clone();
                fixture
                    .connection
                    .discard_pending_rtp(&mid)
                    .expect("discard uncommitted burst");
                let discarded = fixture.connection.stats();
                assert!(queued.connection.queued_media_bytes > 0);
                assert_eq!(discarded.connection.queued_media_bytes, 0);
                assert_eq!(
                    discarded.connection.transmitted_rtp_bytes,
                    committed.connection.transmitted_rtp_bytes
                );
                assert_eq!(
                    discarded.connection.rtp_bytes_in_flight,
                    committed.connection.rtp_bytes_in_flight
                );
                fixture.set_feedback_enabled(true);
            } else {
                fixture.set_desired_bitrate(None);
                fixture.drive_for(Duration::from_secs(2));
                let drained = fixture.connection.stats();
                assert_eq!(
                    drained.senders[0].transmitted_packets - before.senders[0].transmitted_packets,
                    20,
                    "finite burst train must drain after valid feedback: {mode:?}"
                );
                assert_eq!(drained.connection.queued_media_bytes, 0);
                assert_eq!(
                    drained.connection.rtp_bytes_in_flight, 0,
                    "authenticated feedback must retire the tail before history expiry: {mode:?}: {drained:?}"
                );
                assert_eq!(drained.connection.wrong_path_feedback, 0);
            }
            if phase == 0 {
                fixture.set_feedback_enabled(false);
                fixture.drive_for(Duration::from_millis(300));
            }
        }
        assert!(
            fixture.network_counters().2 > 0,
            "duplicate wire traffic exercised"
        );
        assert!(fixture.bottleneck_samples().1 > 0);
        assert_eq!(fixture.connection.stats().connection.feedback, Some(mode));
    }
}

#[test]
fn adaptive_frame_source_tracks_public_estimate_through_impaired_capacity_steps() {
    for mode in [
        pulsebeam_rtc::PacketFeedbackKind::TransportWide,
        pulsebeam_rtc::PacketFeedbackKind::Rfc8888,
    ] {
        adaptive_frame_source(mode);
    }
}

fn adaptive_frame_source(mode: pulsebeam_rtc::PacketFeedbackKind) {
    use pulsebeam_rtc::MediaPayloadBitrate;

    let mut fixture = PeerFixture::connected_with_feedback(mode);
    fixture.configure_network(
        0x2905,
        NetworkPolicy {
            delay: Duration::from_millis(40),
            drop_every: Some(97),
            duplicate_every: Some(41),
            reorder_every: Some(31),
        },
    );
    fixture.configure_bottleneck(1_500_000);
    let demand = 1_000_000_u64;
    fixture.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(demand)));
    let mut sequence = 1;
    let mut credit = 0_u64;
    let mut admitted = 0_u64;
    let mut phase_rates = [0_u64; 3];
    for frame in 0..450_u32 {
        if frame == 150 {
            fixture.set_bottleneck_rate(180_000);
        } else if frame == 300 {
            fixture.set_bottleneck_rate(1_000_000);
        } else if frame == 220 {
            fixture.set_feedback_enabled(false);
        } else if frame == 233 {
            fixture.set_feedback_enabled(true);
        }
        let stats = fixture.connection.stats();
        let rate = stats.connection.bandwidth_estimate.as_bps().min(demand);
        if frame % 150 >= 125 {
            *phase_rates
                .get_mut((frame / 150) as usize)
                .expect("three bounded capacity phases") += rate;
        }
        credit += rate;
        let frame_bytes = credit / 200;
        credit %= 200;
        let mut remaining = frame_bytes;
        while remaining > 0 {
            let bytes = remaining.min(1_000) as usize;
            let mut packet = support::sample(fixture.at(), sequence, bytes, 48_000);
            packet.timestamp = frame * 1_920;
            packet.marker = remaining <= 1_000;
            fixture.write_rtp(packet).expect("bounded frame admission");
            sequence += 1;
            admitted += 1;
            remaining -= bytes as u64;
        }
        fixture.drive_for(Duration::from_millis(40));
        let stats = fixture.connection.stats();
        assert!(stats.connection.queued_media_bytes <= 8 * 1024 * 1024);
        assert!(stats.connection.rtp_bytes_in_flight <= 8 * 1024 * 1024);
        assert!(
            stats
                .senders
                .first()
                .expect("configured sender")
                .transmitted_packets
                <= admitted
        );
    }
    assert!(
        phase_rates[1] < phase_rates[0] && phase_rates[2] > phase_rates[1],
        "source must reduce its nominal budget after the capacity step and recover after relief: {mode:?}: {phase_rates:?}"
    );
    assert!(
        fixture.connection.stats().connection.rtp_bytes_in_flight > 0,
        "recent tail must exercise authenticated retirement: {mode:?}"
    );
    fixture.set_desired_bitrate(None);
    fixture.drive_for(Duration::from_secs(2));
    let stats = fixture.connection.stats();
    assert_eq!(stats.connection.state, pulsebeam_rtc::ConnectionState::Open);
    assert_eq!(stats.connection.close_reason, None);
    assert_eq!(
        stats
            .senders
            .first()
            .expect("configured sender")
            .transmitted_packets,
        admitted
    );
    assert_eq!(stats.connection.queued_media_bytes, 0);
    assert_eq!(stats.connection.rtp_bytes_in_flight, 0);
    assert!(fixture.bottleneck_samples().1 > 0);
    let (_, dropped, duplicated, reordered) = fixture.network_counters();
    assert!(dropped > 0 && duplicated > 0 && reordered > 0);
}

#[derive(Clone, Copy, Debug)]
struct Scenario {
    name: &'static str,
    seed: u64,
    duration: Duration,
    policy: NetworkPolicy,
    packets: u64,
    transport: Transport,
}

#[derive(Clone, Copy, Debug)]
enum Transport {
    Udp,
    Data,
}

const SCENARIOS: [Scenario; 8] = [
    scenario("startup", 0x1101, 10, 40),
    Scenario {
        name: "delay-loss-reorder",
        seed: 0x1102,
        duration: Duration::from_secs(15),
        policy: NetworkPolicy {
            delay: Duration::from_millis(40),
            drop_every: Some(100),
            duplicate_every: None,
            reorder_every: Some(50),
        },
        packets: 512,
        transport: Transport::Udp,
    },
    Scenario {
        name: "periodic-loss",
        seed: 0x1104,
        duration: Duration::from_secs(10),
        policy: NetworkPolicy {
            delay: Duration::from_millis(50),
            drop_every: Some(3),
            duplicate_every: None,
            reorder_every: None,
        },
        packets: 80,
        transport: Transport::Udp,
    },
    Scenario {
        name: "duplication",
        seed: 0x1105,
        duration: Duration::from_secs(10),
        policy: NetworkPolicy {
            delay: Duration::from_millis(25),
            drop_every: None,
            duplicate_every: Some(20),
            reorder_every: None,
        },
        packets: 80,
        transport: Transport::Udp,
    },
    scenario("sender-service", 0x1106, 20, 160),
    Scenario {
        name: "loss-reordering",
        seed: 0x1108,
        duration: Duration::from_secs(12),
        policy: NetworkPolicy {
            delay: Duration::from_millis(25),
            drop_every: Some(2),
            duplicate_every: None,
            reorder_every: Some(1),
        },
        packets: 48,
        transport: Transport::Udp,
    },
    Scenario {
        name: "sctp-coexistence",
        seed: 0x1109,
        duration: Duration::from_secs(10),
        policy: NetworkPolicy {
            delay: Duration::from_millis(40),
            drop_every: None,
            duplicate_every: None,
            reorder_every: None,
        },
        packets: 64,
        transport: Transport::Data,
    },
    Scenario {
        name: "overload-malformed",
        seed: 0x1110,
        duration: Duration::from_secs(1),
        policy: NetworkPolicy {
            delay: Duration::ZERO,
            drop_every: None,
            duplicate_every: None,
            reorder_every: Some(7),
        },
        packets: 64,
        transport: Transport::Udp,
    },
];

const fn scenario(name: &'static str, seed: u64, seconds: u64, packets: u64) -> Scenario {
    Scenario {
        name,
        seed,
        duration: Duration::from_secs(seconds),
        policy: NetworkPolicy {
            delay: Duration::from_millis(25),
            drop_every: None,
            duplicate_every: None,
            reorder_every: None,
        },
        packets,
        transport: Transport::Udp,
    }
}

#[test]
fn fixed_system_matrix() {
    assert_eq!(PAYLOAD_BYTES, 1_000);
    assert_eq!(FEEDBACK_INTERVAL, Duration::from_millis(50));
    let checked_in_trace = include_str!("traces/system-v1.txt");
    assert_eq!(checked_in_trace.lines().count(), SCENARIOS.len());

    for scenario in SCENARIOS {
        assert!(checked_in_trace.contains(scenario.name));
        assert!(checked_in_trace.contains(&format!("{:#06x}", scenario.seed)));
        run(scenario);
    }
}

fn run(scenario: Scenario) {
    let mut fixture = match (scenario.name, scenario.transport) {
        ("sender-service", _) => PeerFixture::connected_with_senders(2),
        (_, Transport::Udp) => PeerFixture::connected(),
        (_, Transport::Data) => PeerFixture::connected_datachannels(),
    };
    if matches!(scenario.transport, Transport::Data) {
        while !matches!(
            fixture.next_data_event(),
            support::FixtureDataEvent::ConnectionOpened(_)
        ) {}
    }
    let mut source = PeerFixture::connected();
    fixture.configure_network(scenario.seed, scenario.policy);

    let mut admitted = 0_u64;
    for id in 1..=scenario.packets {
        source.drive_for(Duration::from_millis(20));
        let packet = source.send_source(&vec![0x5a; PAYLOAD_BYTES]);
        let media = rtp_write(packet, id);
        let sender = if scenario.name == "sender-service" {
            fixture
                .senders
                .get(usize::try_from(id % 2).expect("flow index"))
                .expect("two senders")
                .clone()
        } else {
            fixture.sender_mid.clone()
        };
        for _ in 0..20 {
            match fixture
                .connection
                .write_rtp(fixture.at(), &sender, media.clone())
            {
                Ok(()) => {
                    admitted = admitted.saturating_add(1);
                    break;
                }
                Err(pulsebeam_rtc::RejectedRtp {
                    reason: WriteError::WouldBlock,
                    ..
                }) => {
                    fixture.drive_for(FEEDBACK_INTERVAL);
                }
                Err(error) => panic!(
                    "scenario {} write failed at index {id}: {error:?}",
                    scenario.name
                ),
            }
        }
        fixture.drive_for(Duration::from_millis(1));
    }

    if matches!(scenario.transport, Transport::Data) {
        fixture.peer_send(true, &vec![0xa5; 64 * 1024]);
    }

    if scenario.name == "overload-malformed" {
        fixture
            .connection
            .receive(
                fixture.at(),
                fixture.at(),
                NetworkInput::Udp {
                    local: "127.0.0.1:41000".parse().expect("local address"),
                    remote: "127.0.0.1:9".parse().expect("wrong-path address"),
                    ecn: None,
                    payload: Bytes::from_static(b"malformed"),
                },
            )
            .expect("malformed traffic is isolated");
    }

    fixture.drive_for(scenario.duration);
    let stats = fixture.connection.stats();
    assert!(!stats.senders.is_empty(), "negotiated sender stats");
    let transmitted = stats
        .senders
        .iter()
        .map(|sender| sender.transmitted_packets)
        .sum::<u64>();
    let (network_packets, dropped, duplicated, reordered) = fixture.network_counters();

    let queued = stats
        .senders
        .iter()
        .map(|sender| u64::try_from(sender.queued_packets).expect("queued original count"))
        .sum::<u64>();
    assert!(
        transmitted
            .checked_add(queued)
            .expect("original ownership count")
            <= admitted,
        "transmitted and queued originals cannot exceed admitted writes"
    );
    assert!(stats.connection.queued_media_bytes <= 8 * 1024 * 1024);
    assert!(stats.connection.rtp_bytes_in_flight <= 8 * 1024 * 1024);
    assert!(stats.connection.duplicate_feedback <= stats.connection.transmitted_rtp_bytes);
    assert!(
        stats.connection.transmitted_sctp_bytes == 0
            || matches!(scenario.transport, Transport::Data)
    );
    if matches!(scenario.transport, Transport::Data) {
        assert!(stats.connection.transmitted_sctp_bytes > 0);
    }
    assert!(network_packets >= transmitted);
    if scenario.policy.drop_every.is_some() {
        assert!(dropped > 0, "configured loss must occur");
    }
    if scenario.policy.duplicate_every.is_some() {
        assert!(duplicated > 0, "configured duplication must occur");
    }
    if scenario.policy.reorder_every.is_some() {
        assert!(reordered > 0, "configured reordering must occur");
    }
    if scenario.name == "startup" {
        assert!(transmitted * 100 >= admitted * 85);
    }
    if scenario.name == "sender-service" {
        let delivered = stats
            .senders
            .iter()
            .map(|sender| sender.transmitted_payload_bytes)
            .sum::<u64>();
        let first_share = stats
            .senders
            .first()
            .expect("first competing flow")
            .transmitted_payload_bytes
            * 100
            / delivered.max(1);
        assert!((40..=60).contains(&first_share));
    }
    eprintln!(
        "scenario={} seed={:#06x} duration={:?} packets={} admitted={} transmitted={} network={} dropped={} duplicated={} reordered={} queue_bytes={} bif={} trace={:?}",
        scenario.name,
        scenario.seed,
        scenario.duration,
        scenario.packets,
        admitted,
        transmitted,
        network_packets,
        dropped,
        duplicated,
        reordered,
        stats.connection.queued_media_bytes,
        stats.connection.rtp_bytes_in_flight,
        fixture.network_trace(),
    );

    fixture.connection.abort();
    assert!(matches!(
        fixture.connection.poll(fixture.at()),
        Output::Closed(CloseReason::Aborted)
    ));
    assert!(matches!(
        fixture.connection.poll(fixture.at()),
        Output::Idle { next_wakeup: None }
    ));
}
