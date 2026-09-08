use std::time::Instant;

use pulsebeam_rtc::{
    AcceptError, Connection, ConnectionConfig, ConnectionEntropy, GlobalMediaTime,
    PacketFeedbackKind, SdpOffer, TimePoint,
};

fn accepts(offer: &'static str) -> Result<(), AcceptError> {
    let accepted = Connection::accept(
        ConnectionConfig::default(),
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
