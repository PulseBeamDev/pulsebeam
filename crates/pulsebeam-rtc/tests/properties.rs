#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    reason = "property tests use checked bounded domains and fail with their generated trace"
)]

mod support;

use std::time::Duration;

use pulsebeam_rtc::Output;
use support::{PeerFixture, rtp_write};

#[test]
fn commit_and_terminal_output_are_atomic_and_once_only() {
    let mut fixture = PeerFixture::connected();
    let packet = fixture.send_source(b"commit");
    fixture
        .write_rtp(rtp_write(packet, 1))
        .expect("write direct media");
    let before = fixture.connection.stats();
    let _ = fixture.receive_egress();
    let committed = fixture.connection.stats();
    assert_eq!(
        committed.senders[0].transmitted_packets,
        before.senders[0].transmitted_packets + 1
    );
    assert!(committed.connection.transmitted_rtp_bytes > before.connection.transmitted_rtp_bytes);

    fixture.connection.abort();
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
            fixture
                .write_rtp(rtp_write(packet.clone(), id))
                .expect("write direct media");
        }
        fixture.drive_for(Duration::from_secs(2));
        (fixture.network_counters(), fixture.network_trace().to_vec())
    }

    assert_eq!(run(), run(), "seed=0x11fe");
}
