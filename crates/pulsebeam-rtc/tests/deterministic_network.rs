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
use pulsebeam_rtc::{CloseReason, Command, CommandError, NetworkInput, Output};
use support::{NetworkPolicy, PeerFixture, forwarded};

const PAYLOAD_BYTES: usize = 1_000;
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(50);

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
    Tcp,
    Data,
}

const SCENARIOS: [Scenario; 11] = [
    scenario("startup", 0x1101, 10, 40),
    Scenario {
        name: "vbr-keyframe",
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
    scenario("bandwidth-rtt-steps", 0x1103, 15, 96),
    Scenario {
        name: "feedback-outage",
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
        name: "ecn",
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
    scenario("policer-competition", 0x1106, 20, 160),
    scenario("pause-switch", 0x1107, 12, 48),
    Scenario {
        name: "path-replacement",
        seed: 0x1108,
        duration: Duration::from_secs(12),
        policy: NetworkPolicy {
            delay: Duration::from_millis(25),
            drop_every: Some(2),
            duplicate_every: None,
            reorder_every: Some(1),
        },
        packets: 48,
        transport: Transport::Tcp,
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
    scenario("close", 0x1111, 2, 32),
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
        ("policer-competition", _) => PeerFixture::connected_with_senders(2),
        (_, Transport::Udp) => PeerFixture::connected(),
        (_, Transport::Tcp) => PeerFixture::connected_tcp(),
        (_, Transport::Data) => PeerFixture::connected_datachannels(),
    };
    if matches!(scenario.transport, Transport::Data) {
        while !matches!(
            fixture.next_data_event(),
            support::FixtureDataEvent::ConnectionOpened(_)
        ) {}
    }
    let mut source = PeerFixture::connected();
    let mut alternate_source = PeerFixture::connected();
    fixture.configure_network(scenario.seed, scenario.policy);

    let mut admitted = 0_u64;
    for id in 1..=scenario.packets {
        source.drive_for(Duration::from_millis(20));
        alternate_source.drive_for(Duration::from_millis(20));
        let selected_source = if scenario.name == "pause-switch" && id > scenario.packets / 2 {
            &mut alternate_source
        } else {
            &mut source
        };
        let packet = selected_source.send_source(&vec![0x5a; PAYLOAD_BYTES]);
        let media = forwarded(packet, id);
        let sender = if scenario.name == "policer-competition" {
            *fixture
                .senders
                .get(usize::try_from(id % 2).expect("flow index"))
                .expect("two competing flows")
        } else {
            fixture.sender
        };
        for _ in 0..20 {
            match fixture.connection.command(
                fixture.at(),
                Command::SendMedia {
                    sender,
                    media: media.clone(),
                },
            ) {
                Ok(()) => {
                    admitted = admitted.saturating_add(1);
                    break;
                }
                Err(CommandError::WouldBlock) => fixture.drive_for(FEEDBACK_INTERVAL),
                Err(error) => panic!("scenario admission failed: {error:?}"),
            }
        }
    }

    if matches!(scenario.transport, Transport::Data) {
        fixture.peer_send(true, &vec![0xa5; 64 * 1024]);
    }

    if scenario.name == "overload-malformed" {
        fixture
            .connection
            .receive(
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
    let sender = stats.senders.first().expect("negotiated sender stats");
    let transmitted = stats
        .senders
        .iter()
        .map(|sender| sender.transmitted_packets)
        .sum::<u64>();
    let (network_packets, dropped, duplicated, reordered) = fixture.network_counters();

    assert!(transmitted <= admitted);
    assert!(sender.queued_packets <= 8_192);
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
    if scenario.name == "policer-competition" {
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

    fixture.command(Command::Abort);
    assert!(matches!(
        fixture.connection.poll(fixture.at()),
        Output::Closed(CloseReason::Aborted)
    ));
    assert!(matches!(
        fixture.connection.poll(fixture.at()),
        Output::Idle { next_wakeup: None }
    ));
}
