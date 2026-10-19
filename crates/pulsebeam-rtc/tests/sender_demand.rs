#![allow(
    clippy::expect_used,
    reason = "public demand fixtures stop at violated protocol invariants"
)]

mod support;

use std::time::Duration;

use pulsebeam_rtc::{Command, CommandError, MediaPayloadBitrate, SenderDemand};
use support::{PeerFixture, forwarded};

#[test]
fn default_playout_preserves_caller_target_and_forwards_under_native_capacity() {
    let mut fixture = PeerFixture::connected();
    let selected = MediaPayloadBitrate::from_bps(100_000);
    assert!(fixture.connection.stats().connection.media_budget >= selected);
    let demand = SenderDemand::new(selected, selected).expect("fixed source demand");
    fixture.command(Command::SetSenderDemand {
        sender: fixture.sender,
        demand,
    });
    let stats = fixture.connection.stats();
    let sender = stats.senders.first().expect("sender");
    assert_eq!(sender.policy.playout_delay, None);
    assert!(stats.connection.media_budget >= selected);
    assert_eq!(sender.demand, demand);
    let packet = fixture.send_source(b"fully funded default source");
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: forwarded(packet, 1),
    });
    let (_, payload) = fixture.receive_egress();
    assert_eq!(payload, b"fully funded default source");
}

#[test]
fn potential_demand_does_not_admit_paused_media_and_selected_demand_can_resume() {
    let mut fixture = PeerFixture::connected();
    let paused = SenderDemand::new(
        MediaPayloadBitrate::from_bps(0),
        MediaPayloadBitrate::from_bps(2_000_000),
    )
    .expect("paused source interest");
    let padding_before = fixture
        .connection
        .stats()
        .connection
        .transmitted_padding_bytes;
    fixture.command(Command::SetSenderDemand {
        sender: fixture.sender,
        demand: paused,
    });
    let packet = fixture.send_source(b"paused source");
    assert_eq!(
        fixture.connection.command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(packet, 1),
            }
        ),
        Err(CommandError::WouldBlock)
    );
    fixture.drive_for(Duration::from_millis(100));
    let stats = fixture.connection.stats();
    let sender = stats.senders.first().expect("sender");
    assert_eq!(sender.demand, paused);
    assert_eq!(sender.policy.desired_bitrate.as_bps(), 0);
    assert_eq!(sender.transmitted_payload_bytes, 0);
    assert!(stats.connection.transmitted_padding_bytes > padding_before);
    assert!(stats.connection.media_budget.as_bps() < paused.potential().as_bps());
    assert!(stats.connection.media_budget.as_bps() > 0);
    assert!(stats.connection.media_budget.as_bps() < paused.potential().as_bps());

    let selected = SenderDemand::new(MediaPayloadBitrate::from_bps(256_000), paused.potential())
        .expect("selected source");
    fixture.command(Command::SetSenderDemand {
        sender: fixture.sender,
        demand: selected,
    });
    let packet = fixture.send_source(b"selected source resumed");
    let mut media = forwarded(packet, 2);
    let mut admitted = false;
    for _ in 0..100 {
        match fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media: media.clone(),
        }) {
            Ok(()) => {
                admitted = true;
                break;
            }
            Err(CommandError::WouldBlock) => {
                fixture.drive_for(Duration::from_millis(2));
                let fresh = fixture.send_source(b"selected source resumed");
                media = forwarded(fresh, 2);
            }
            Err(error) => panic!("unexpected resume error: {error:?}"),
        }
    }
    assert!(
        admitted,
        "selected media must resume after bounded probe debt"
    );
    let (_, payload) = fixture.receive_egress();
    assert_eq!(payload, b"selected source resumed");
    assert_eq!(
        fixture
            .connection
            .stats()
            .senders
            .first()
            .expect("sender")
            .demand,
        selected
    );
}
