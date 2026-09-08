use std::{net::SocketAddr, time::Instant};

use pulsebeam_rtc::{
    AcceptError, Connection, ConnectionConfig, ConnectionEntropy, GlobalMediaTime, LocalCandidate,
    PacketFeedbackKind, SdpOffer, TimePoint,
};

fn accepts(offer: &'static str) -> Result<(), AcceptError> {
    let accepted = Connection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(SocketAddr::from((
                [192, 0, 2, 1],
                5000,
            )))],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(offer),
        TimePoint {
            monotonic: Instant::now(),
            global: GlobalMediaTime::from_micros(1),
        },
        ConnectionEntropy::new([0x5a; 32]),
    )?;
    assert_eq!(
        accepted.session.feedback,
        Some(PacketFeedbackKind::TransportWide)
    );
    assert!(accepted.answer.as_str().contains("a=ice-lite"));
    assert!(
        accepted
            .answer
            .as_str()
            .contains(" 192.0.2.1 5000 typ host")
    );
    Ok(())
}

#[test]
fn chrome_representative_offer_is_compatible() -> Result<(), AcceptError> {
    accepts(include_str!("fixtures/chrome-representative.sdp"))
}

#[test]
fn firefox_representative_offer_is_compatible() -> Result<(), AcceptError> {
    accepts(include_str!("fixtures/firefox-representative.sdp"))
}
