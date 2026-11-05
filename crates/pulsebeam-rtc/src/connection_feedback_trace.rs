use super::*;
use crate::{
    TransmitKind,
    test_support::{PeerFixture, sample},
};
use std::{fmt::Write as _, time::Duration};

#[test]
fn feedback_committed_transmit_baseline() {
    let mut trace = String::new();
    for mode in [
        PacketFeedbackKind::TransportWide,
        PacketFeedbackKind::Rfc8888,
    ] {
        let mut fixture = PeerFixture::connected_with_feedback(mode).without_egress_observation();
        fixture.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(2_000_000)));
        fixture.configure_rtc_wakeup_quantum(Duration::from_millis(1));
        fixture.record_transmits();
        fixture.record_idle_wakeups();
        let origin = fixture.at();
        writeln!(trace, "{mode:?}").unwrap();
        let mut prior_emissions = 0;
        for round in 0..8 {
            if round < 4 {
                for offset in 0..8 {
                    fixture
                        .write_rtp(sample(fixture.at(), round * 8 + offset, 500, 48_000))
                        .unwrap();
                }
            }
            fixture.drive_for(Duration::from_millis(40));
            let stats = fixture.connection.stats().connection;
            writeln!(
                trace,
                "at={:?} committed={} padding={} queued={} flight={} counters={:?}",
                fixture.at().saturating_duration_since(origin),
                stats.transmitted_rtp_bytes,
                stats.transmitted_padding_bytes,
                stats.queued_media_bytes,
                fixture
                    .connection
                    .commit
                    .history
                    .controller_inputs()
                    .bytes_in_flight,
                fixture.connection.commit.history.counters()
            )
            .unwrap();
            writeln!(
                trace,
                "  controller={:?}",
                fixture.connection.egress.test_envelope()
            )
            .unwrap();
            let emissions = fixture
                .emitted_rtp()
                .iter()
                .skip(prior_emissions)
                .map(|(at, bytes, padding)| {
                    (at.saturating_duration_since(origin), *bytes, *padding)
                })
                .collect::<Vec<_>>();
            prior_emissions = fixture.emitted_rtp().len();
            writeln!(trace, "  emissions={emissions:?}").unwrap();
            let transmits = fixture
                .take_transmit_observations()
                .into_iter()
                .map(|(at, kind)| {
                    let kind = match kind {
                        TransmitKind::Original { .. } => "original",
                        TransmitKind::Repair { .. } => "repair",
                        TransmitKind::Padding { .. } => "padding",
                        TransmitKind::Control => "control",
                    };
                    (at.saturating_duration_since(origin), kind)
                })
                .collect::<Vec<_>>();
            writeln!(trace, "  transmits={transmits:?}").unwrap();
            let idle = fixture
                .take_idle_observations()
                .into_iter()
                .map(|(at, next)| {
                    (
                        at.saturating_duration_since(origin),
                        next.map(|next| next.saturating_duration_since(origin)),
                    )
                })
                .collect::<Vec<_>>();
            writeln!(trace, "  idle={idle:?}").unwrap();
        }
        assert!(
            fixture.connection.commit.history.counters().received > 0,
            "authenticated {mode:?} feedback must reach the real connection"
        );
    }
    assert_eq!(
        trace,
        include_str!("../tests/traces/feedback-commits.trace")
    );
}
