use std::{
    thread,
    time::{Duration, Instant},
};

use pulsebeam_webrtc_sys::*;

fn audio_offer(peer: &PeerConnection) -> SessionDescription {
    let operation = peer
        .create_offer_with_options(OfferAnswerOptions {
            receive_audio: Some(1),
            ..Default::default()
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::OperationComplete(done) = event
                && done.operation_id == operation
            {
                return done.result.unwrap().unwrap();
            }
        }
        assert!(Instant::now() < deadline, "audio offer did not complete");
        thread::yield_now();
    }
}

fn assert_encryption(offer: &SessionDescription, encrypted: bool) {
    assert_eq!(
        offer.sdp.contains("a=fingerprint:"),
        encrypted,
        "{}",
        offer.sdp
    );
    let protocol = if encrypted {
        "UDP/TLS/RTP/SAVPF"
    } else {
        "RTP/AVPF"
    };
    assert!(offer.sdp.contains(protocol), "{}", offer.sdp);
}

#[test]
fn factory_options_apply_to_new_peers_without_reconfiguring_existing_peers() {
    let insecure = FactoryOptions {
        disable_encryption: true,
        disable_network_monitor: true,
        ignored_network_adapters: NetworkAdapterMask::NONE,
        ssl_max_version: SslProtocolVersion::DTLS_12,
    };
    let factory = PeerConnectionFactory::builder()
        .options(insecure)
        .build()
        .unwrap();
    let first = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    factory.set_options(FactoryOptions::default());
    let second = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    factory.set_options(insecure);
    let third = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    // Generate after both updates: the engine, not a Rust option echo, must
    // preserve each peer's construction-time security configuration.
    assert_encryption(&audio_offer(&first), false);
    assert_encryption(&audio_offer(&second), true);
    assert_encryption(&audio_offer(&third), false);
}

#[test]
fn signed_and_legacy_protocol_selectors_are_accepted_without_narrowing() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    for ssl_max_version in [
        SslProtocolVersion::Unspecified,
        SslProtocolVersion::Tls10,
        SslProtocolVersion::DTLS_10,
        SslProtocolVersion::DTLS_12,
        SslProtocolVersion::DTLS_13,
    ] {
        factory.set_options(FactoryOptions {
            ssl_max_version,
            // Unknown flag bits are open native values, including the sign bit
            // of native int. They do not accidentally select a known category.
            ignored_network_adapters: NetworkAdapterMask::LOOPBACK
                | NetworkAdapterMask::from_bits(1 << 31),
            ..Default::default()
        });
        let peer = factory
            .create_peer_connection(PeerConfiguration::default())
            .unwrap();
        assert_encryption(&audio_offer(&peer), true);
    }
}

fn actor_audio_offer(session: &mut ProductionSession, peer: SessionPeerId) -> SessionDescription {
    let operation = session
        .create_offer_with_options(
            peer,
            OfferAnswerOptions {
                receive_audio: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        while let Some(event) = session.try_peer_event(peer).unwrap() {
            if let SessionEvent::OperationComplete(done) = event
                && done.operation_id == operation
            {
                return done.result.unwrap().unwrap();
            }
        }
        assert!(
            Instant::now() < deadline,
            "actor audio offer did not complete"
        );
        thread::yield_now();
    }
}

#[test]
fn actor_factory_options_remain_effective_after_migration_and_fail_after_shutdown() {
    let mut session = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    session
        .set_factory_options(FactoryOptions {
            disable_encryption: true,
            ..Default::default()
        })
        .unwrap();
    let insecure = session.create_peer(PeerConfiguration::default()).unwrap();
    let session = thread::spawn(move || {
        let mut session = session;
        session
            .set_factory_options(FactoryOptions::default())
            .unwrap();
        let secure = session.create_peer(PeerConfiguration::default()).unwrap();
        assert_encryption(&actor_audio_offer(&mut session, insecure), false);
        assert_encryption(&actor_audio_offer(&mut session, secure), true);
        session
    })
    .join()
    .unwrap();
    thread::spawn(move || {
        let mut session = session;
        session.shutdown().unwrap();
        assert_eq!(
            session
                .set_factory_options(FactoryOptions::default())
                .unwrap_err()
                .kind,
            PeerErrorKind::Closed
        );
    })
    .join()
    .unwrap();
}
