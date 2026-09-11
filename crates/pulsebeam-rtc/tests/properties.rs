#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    reason = "property tests use checked bounded domains and fail with their generated trace"
)]

mod support;

use std::time::Duration;

use proptest::prelude::*;
use pulsebeam_rtc::{
    Command, GlobalMediaTime, MediaPayloadBitrate, MediaPriority, Output, PlayoutDelay,
    SenderPolicy,
};
use support::{PeerFixture, forwarded};

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::WithSource("properties-regressions"))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn global_time_wire_round_trip_is_exact(value in any::<u64>()) {
        let time = GlobalMediaTime::from_micros(value);
        prop_assert_eq!(GlobalMediaTime::from_be_bytes(time.to_be_bytes()), time);
    }

    #[test]
    fn checked_global_time_arithmetic_never_wraps(value in any::<u64>(), delta in any::<u32>()) {
        let time = GlobalMediaTime::from_micros(value);
        let duration = Duration::from_micros(u64::from(delta));
        match time.checked_add(duration) {
            Some(later) => {
                prop_assert!(later >= time);
                prop_assert_eq!(later.checked_duration_since(time), Some(duration));
            }
            None => prop_assert!(value.checked_add(u64::from(delta)).is_none()),
        }
    }

    #[test]
    fn playout_policy_is_monotonic_over_the_complete_wire_domain(
        first in 0_u16..=4095,
        second in 0_u16..=4095,
    ) {
        let (tighter, looser) = if first <= second { (first, second) } else { (second, first) };
        let tight = PlayoutDelay::from_ticks(0, tighter).expect("generated valid policy");
        let loose = PlayoutDelay::from_ticks(0, looser).expect("generated valid policy");
        prop_assert!(tight.max() <= loose.max());
    }
}

#[test]
fn allocation_is_conserving_demand_capped_and_priority_monotonic_through_stats() {
    let mut source = PeerFixture::connected();
    let packet = source.send_source(b"allocation");
    let mut fixture = PeerFixture::connected_with_senders(3);
    let priorities = [
        MediaPriority::VERY_LOW,
        MediaPriority::LOW,
        MediaPriority::HIGH,
    ];
    let senders = fixture.senders.clone();
    for (index, (sender, priority)) in senders.into_iter().zip(priorities).enumerate() {
        fixture.command(Command::SetSenderPolicy {
            sender,
            policy: SenderPolicy {
                playout_delay: PlayoutDelay::from_ticks(0, 400).expect("valid playout"),
                priority,
                desired_bitrate: MediaPayloadBitrate::from_bps(4_000_000),
            },
        });
        fixture.command(Command::SendMedia {
            sender,
            media: forwarded(
                packet.clone(),
                u64::try_from(index).expect("small index") + 1,
            ),
        });
    }
    fixture.drive_for(Duration::from_millis(500));

    let stats = fixture.connection.stats();
    let allocations = stats
        .senders
        .iter()
        .map(|sender| sender.allocation.as_bps())
        .collect::<Vec<_>>();
    assert_eq!(allocations.len(), 3);
    assert!(allocations[0] <= allocations[1]);
    assert!(allocations[1] <= allocations[2]);
    assert!(
        allocations
            .iter()
            .all(|allocation| *allocation <= 4_000_000)
    );
    assert!(allocations.iter().sum::<u64>() <= stats.connection.target_media_bitrate.as_bps());
}

#[test]
fn commit_and_terminal_output_are_atomic_and_once_only() {
    let mut fixture = PeerFixture::connected();
    let packet = fixture.send_source(b"commit");
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: forwarded(packet, 1),
    });
    let before = fixture.connection.stats();
    let _ = fixture.receive_egress();
    let committed = fixture.connection.stats();
    assert_eq!(
        committed.senders[0].transmitted_packets,
        before.senders[0].transmitted_packets + 1
    );
    assert!(committed.connection.transmitted_rtp_bytes > before.connection.transmitted_rtp_bytes);

    fixture.command(Command::Abort);
    assert!(matches!(
        fixture.connection.poll(fixture.at()),
        Output::Closed(_)
    ));
    for _ in 0..4 {
        assert!(matches!(
            fixture.connection.poll(fixture.at()),
            Output::Idle { next_wakeup: None }
        ));
    }
}

#[test]
fn bounded_network_trace_replays_exactly_for_the_recorded_seed() {
    fn run() -> ((u64, u64, u64, u64), Vec<&'static str>) {
        let mut fixture = PeerFixture::connected();
        let packet = fixture.send_source(b"trace");
        fixture.configure_network(
            0x11fe,
            support::NetworkPolicy {
                delay: Duration::from_millis(10),
                drop_every: Some(7),
                duplicate_every: Some(11),
                reorder_every: Some(5),
            },
        );
        for id in 1..=3 {
            fixture.command(Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(packet.clone(), id),
            });
        }
        fixture.drive_for(Duration::from_secs(2));
        (fixture.network_counters(), fixture.network_trace().to_vec())
    }

    assert_eq!(run(), run(), "seed=0x11fe");
}
