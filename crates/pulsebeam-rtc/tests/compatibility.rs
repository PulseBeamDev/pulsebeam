use std::{net::SocketAddr, time::Instant};

use pulsebeam_rtc::{
    AcceptError, Connection, ConnectionConfig, ConnectionEntropy, LocalCandidate,
    PacketFeedbackKind, SdpOffer,
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
        Instant::now(),
        std::time::UNIX_EPOCH,
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
fn parser_accepts_bundle_with_named_mids() -> Result<(), AcceptError> {
    accepts(include_str!("fixtures/chrome-representative.sdp"))
}

#[test]
fn parser_accepts_bundle_with_numeric_mids() -> Result<(), AcceptError> {
    accepts(include_str!("fixtures/firefox-representative.sdp"))
}
