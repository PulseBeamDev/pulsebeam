#![allow(
    clippy::expect_used,
    reason = "integration fixtures stop at violated public protocol invariants"
)]

mod support;

use std::time::Duration;

use pulsebeam_rtc::{Command, CommandError, PlayoutDelay, PlayoutDelayStatus};
use support::{PeerFixture, forwarded};

fn video_fixture() -> PeerFixture {
    let mut fixture = PeerFixture::connected_video();
    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: Some({
            let (selected_bitrate, potential_bitrate) = (
                pulsebeam_rtc::MediaPayloadBitrate::from_bps(1_000_000),
                pulsebeam_rtc::MediaPayloadBitrate::from_bps(1_000_000),
            );
            pulsebeam_rtc::SenderAssignment {
                selected_bitrate,
                potential_bitrate,
                ..fixture.assignment(fixture.sender)
            }
        }),
    });
    fixture
}

fn send(fixture: &mut PeerFixture, id: u64) -> support::FixtureRtpHeader {
    // Match the controlled native peer's approximately 30 fps capture cadence.
    fixture.drive_for(Duration::from_millis(34));
    let packet = fixture.send_source(b"\x65playout media");
    fixture
        .admit_media(forwarded(packet, id))
        .expect("media admission");
    fixture.receive_video_wire_header()
}

fn policy(fixture: &mut PeerFixture, delay: Option<PlayoutDelay>) {
    let mut policy = fixture
        .connection
        .stats()
        .senders
        .first()
        .expect("sender")
        .assignment
        .expect("bound sender");
    policy.playout_delay = delay;
    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: Some(policy),
    });
}

fn status(fixture: &PeerFixture) -> PlayoutDelayStatus {
    fixture
        .connection
        .stats()
        .senders
        .first()
        .expect("sender")
        .playout_delay
}

#[test]
fn browser_default_and_explicit_zero_have_distinct_wire_behavior() {
    let mut fixture = video_fixture();
    assert_eq!(status(&fixture), PlayoutDelayStatus::Unspecified);
    let header = send(&mut fixture, 1);
    assert_eq!(header.ext_vals.play_delay_min, None);
    assert_eq!(header.ext_vals.play_delay_max, None);

    policy(&mut fixture, Some(PlayoutDelay::ZERO));
    let header = send(&mut fixture, 2);
    assert_eq!(
        header
            .ext_vals
            .play_delay_min
            .map(|value| value.as_micros()),
        Some(0)
    );
    assert_eq!(
        header
            .ext_vals
            .play_delay_max
            .map(|value| value.as_micros()),
        Some(0)
    );
    assert_eq!(status(&fixture), PlayoutDelayStatus::Pending);
    let mut default = fixture
        .connection
        .stats()
        .senders
        .first()
        .expect("sender")
        .assignment
        .expect("bound sender");
    default.playout_delay = None;
    assert_eq!(
        fixture.connection.command(
            fixture.at(),
            Command::SetSenderAssignment {
                sender: fixture.sender,
                assignment: Some(default)
            }
        ),
        Err(CommandError::PlayoutResetRequiresNewSender(fixture.sender))
    );
}

#[test]
fn explicit_delay_repeats_until_authenticated_receiver_report_and_resets_on_change() {
    let mut fixture = video_fixture();
    let delay = PlayoutDelay::from_millis(149, 204).expect("millisecond policy");
    policy(&mut fixture, Some(delay));
    for id in 1..=2 {
        let header = send(&mut fixture, id);
        assert_eq!(
            header
                .ext_vals
                .play_delay_min
                .map(|value| value.as_micros()),
            Some(150_000)
        );
        assert_eq!(
            header
                .ext_vals
                .play_delay_max
                .map(|value| value.as_micros()),
            Some(200_000)
        );
        assert_eq!(status(&fixture), PlayoutDelayStatus::Pending);
    }
    assert_eq!(
        fixture
            .connection
            .stats()
            .senders
            .first()
            .expect("sender")
            .assignment
            .expect("bound sender")
            .playout_delay,
        Some(delay)
    );
    fixture.drive_for(Duration::from_millis(200));
    assert!(fixture.twcc_sent() > 0);
    assert_eq!(status(&fixture), PlayoutDelayStatus::Pending);
    // Confirmation must come from authenticated receiver reports, not header observation.
    fixture.drive_for(Duration::from_secs(6));
    assert_eq!(status(&fixture), PlayoutDelayStatus::Confirmed);
    let header = send(&mut fixture, 3);
    assert_eq!(header.ext_vals.play_delay_min, None);
    assert_eq!(header.ext_vals.play_delay_max, None);

    fixture.drive_for(Duration::from_millis(34));
    let packet = fixture.send_source(b"\x65playout media");
    policy(
        &mut fixture,
        Some(PlayoutDelay::from_ticks(2, 30).expect("updated policy")),
    );
    assert_eq!(status(&fixture), PlayoutDelayStatus::Pending);
    // Admit before driving: recovery padding can also carry and confirm the policy.
    fixture
        .connection
        .command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(packet, 4),
            },
        )
        .expect("media admission");
    let header = fixture.receive_video_wire_header();
    assert_eq!(
        header
            .ext_vals
            .play_delay_min
            .map(|value| value.as_micros()),
        Some(20_000)
    );
    assert_eq!(
        header
            .ext_vals
            .play_delay_max
            .map(|value| value.as_micros()),
        Some(300_000)
    );
}

#[test]
fn unspecified_and_zero_wire_delay_respect_explicit_media_age() {
    for delay in [None, Some(PlayoutDelay::ZERO)] {
        let mut fixture = PeerFixture::connected();
        policy(&mut fixture, delay);
        let mut assignment = fixture.assignment(fixture.sender);
        assignment.delivery.max_media_age = Duration::from_millis(100);
        fixture.command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some(assignment),
        });
        let packet = fixture.send_source(b"expired media");
        fixture.drive_for(Duration::from_millis(200));
        assert_eq!(
            fixture.connection.command(
                fixture.at(),
                Command::SendMedia {
                    sender: fixture.sender,
                    media: forwarded(packet, 1),
                }
            ),
            Err(CommandError::MediaExpired)
        );
        assert_eq!(
            fixture
                .connection
                .stats()
                .senders
                .first()
                .expect("sender")
                .queued_packets,
            0
        );
        assert_eq!(
            status(&fixture),
            if delay.is_none() {
                PlayoutDelayStatus::Unspecified
            } else {
                PlayoutDelayStatus::NotSignaled
            }
        );
        policy(&mut fixture, None);
    }
}
