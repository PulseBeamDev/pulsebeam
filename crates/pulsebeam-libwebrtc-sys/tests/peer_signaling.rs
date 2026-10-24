use std::{
    thread,
    time::{Duration, Instant},
};

use pulsebeam_webrtc_sys::*;

fn complete(
    peer: &PeerConnection,
    operation: OperationId,
) -> Result<Option<SessionDescription>, PeerError> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        while let Some(event) = peer.try_next_event() {
            if let PeerConnectionEvent::OperationComplete(done) = event
                && done.operation_id == operation
            {
                return done.result;
            }
        }
        assert!(
            Instant::now() < deadline,
            "operation {} did not complete",
            operation.get()
        );
        thread::yield_now();
    }
}

fn candidate(description: &SessionDescription) -> IceCandidate {
    IceCandidate {
        sdp_mid: description
            .sdp
            .lines()
            .find_map(|line| line.strip_prefix("a=mid:"))
            .unwrap()
            .into(),
        sdp_mline_index: 0,
        candidate: "candidate:1 1 UDP 2122260223 192.0.2.1 5000 typ host".into(),
    }
}

#[test]
fn native_offer_options_are_effective_and_failures_remain_typed() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let _channel = peer
        .create_data_channel("options", DataChannelConfiguration::default())
        .unwrap();
    let default = complete(&peer, peer.create_offer().unwrap())
        .unwrap()
        .unwrap();
    assert!(
        default.sdp.contains("UDP/DTLS/SCTP webrtc-datachannel"),
        "{}",
        default.sdp
    );
    assert!(default.sdp.contains("a=group:BUNDLE"));
    let legacy = complete(
        &peer,
        peer.create_offer_with_options(OfferAnswerOptions {
            use_rtp_mux: false,
            use_obsolete_sctp_sdp: true,
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap()
    .unwrap();
    assert!(legacy.sdp.contains("DTLS/SCTP 5000"), "{}", legacy.sdp);
    assert!(!legacy.sdp.contains("a=group:BUNDLE"));
    let invalid = peer
        .create_offer_with_options(OfferAnswerOptions {
            receive_audio: Some(2),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        complete(&peer, invalid).unwrap_err().kind,
        PeerErrorKind::InvalidParameter
    );
    assert_eq!(
        peer.create_offer_with_options(OfferAnswerOptions {
            simulcast_layers: u32::MAX,
            ..Default::default()
        })
        .unwrap_err()
        .kind,
        PeerErrorKind::InvalidRange
    );
    // A rejected option does not poison subsequent native generation.
    assert!(
        complete(&peer, peer.create_offer().unwrap())
            .unwrap()
            .is_some()
    );
    peer.close().unwrap();
    assert_eq!(peer.restart_ice().unwrap_err().kind, PeerErrorKind::Closed);
    assert_eq!(
        peer.set_local_description_auto().unwrap_err().kind,
        PeerErrorKind::Closed
    );
    assert_eq!(
        peer.should_fire_negotiation_needed(0).unwrap_err().kind,
        PeerErrorKind::Closed
    );
    assert_eq!(
        peer.add_ice_candidate_sync(candidate(&default))
            .unwrap_err()
            .kind,
        PeerErrorKind::Closed
    );
    assert_eq!(
        peer.remove_ice_candidate(candidate(&default))
            .unwrap_err()
            .kind,
        PeerErrorKind::Closed
    );
}

#[test]
fn automatic_local_sdp_sync_candidates_and_direct_restart_preserve_native_state() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut alice = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    let mut bob = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    assert_eq!(alice.signaling_state(), SignalingState::Stable);
    assert_eq!(alice.peer_connection_state(), ConnectionState::New);
    assert_eq!(alice.ice_gathering_state(), IceGatheringState::New);
    let _channel = alice
        .create_data_channel("auto", DataChannelConfiguration::default())
        .unwrap();
    assert!(!alice.should_fire_negotiation_needed(u32::MAX).unwrap());
    let invalid = IceCandidate {
        sdp_mid: "0".into(),
        sdp_mline_index: 0,
        candidate: "not a candidate".into(),
    };
    let error = bob.add_ice_candidate_sync(invalid.clone()).unwrap_err();
    assert_eq!(error.kind, PeerErrorKind::Syntax);
    assert!(!error.message.is_empty());
    assert_eq!(
        bob.remove_ice_candidate(invalid).unwrap_err().kind,
        PeerErrorKind::Syntax
    );
    assert_eq!(
        bob.add_ice_candidate_sync(IceCandidate {
            sdp_mid: "0".into(),
            sdp_mline_index: 0,
            candidate: "candidate:1 1 UDP 2122260223 192.0.2.1 5000 typ host".into()
        })
        .unwrap(),
        IceCandidateDisposition::Rejected
    );

    assert_eq!(
        complete(&alice, alice.set_local_description_auto().unwrap()).unwrap(),
        None
    );
    assert_eq!(alice.signaling_state(), SignalingState::HaveLocalOffer);
    let descriptions = alice.descriptions().unwrap();
    assert!(descriptions.current_local.is_none());
    let offer = descriptions.local().unwrap().clone();
    assert_eq!(offer.kind, SessionDescriptionType::Offer);
    complete(&bob, bob.set_remote_description(offer.clone()).unwrap()).unwrap();
    assert_eq!(bob.signaling_state(), SignalingState::HaveRemoteOffer);
    assert_eq!(bob.descriptions().unwrap().remote(), Some(&offer));
    assert_eq!(
        bob.add_ice_candidate_sync(candidate(&offer)).unwrap(),
        IceCandidateDisposition::Accepted
    );
    assert_eq!(
        bob.remove_ice_candidate(candidate(&offer)).unwrap(),
        IceCandidateDisposition::Accepted
    );
    assert_eq!(
        complete(&bob, bob.set_local_description_auto().unwrap()).unwrap(),
        None
    );
    let answer = bob.descriptions().unwrap().local().unwrap().clone();
    assert_eq!(answer.kind, SessionDescriptionType::Answer);
    complete(&alice, alice.set_remote_description(answer).unwrap()).unwrap();
    assert_eq!(alice.signaling_state(), SignalingState::Stable);
    assert_eq!(bob.signaling_state(), SignalingState::Stable);
    let stable = alice.descriptions().unwrap();
    assert!(stable.pending_local.is_none());
    assert_eq!(stable.local(), stable.current_local.as_ref());
    alice.restart_ice().unwrap();
    let restart = complete(&alice, alice.create_offer().unwrap())
        .unwrap()
        .unwrap();
    let ufrag = |sdp: &str| {
        sdp.lines()
            .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
            .unwrap()
            .to_owned()
    };
    assert_ne!(ufrag(&offer.sdp), ufrag(&restart.sdp));
    alice.close().unwrap();
    bob.close().unwrap();
    assert_eq!(alice.signaling_state(), SignalingState::Closed);
    assert_eq!(bob.peer_connection_state(), ConnectionState::Closed);
}

#[test]
fn retired_and_replacement_endpoints_with_same_native_id_remain_distinct() {
    fn negotiate(alice: &PeerConnection, bob: &PeerConnection) {
        let offer = complete(alice, alice.create_offer().unwrap())
            .unwrap()
            .unwrap();
        complete(alice, alice.set_local_description(offer.clone()).unwrap()).unwrap();
        complete(bob, bob.set_remote_description(offer).unwrap()).unwrap();
        let answer = complete(bob, bob.create_answer().unwrap())
            .unwrap()
            .unwrap();
        complete(bob, bob.set_local_description(answer.clone()).unwrap()).unwrap();
        complete(alice, alice.set_remote_description(answer).unwrap()).unwrap();
    }
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut alice = factory.create_peer_connection(Default::default()).unwrap();
    let mut bob = factory.create_peer_connection(Default::default()).unwrap();
    let source = factory.create_video_source().unwrap();
    let track = factory
        .create_video_track("reused-endpoint-id", &source)
        .unwrap();
    assert!(track.set_enabled(false));
    let retired = alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let old_sender = retired.sender();
    let old_receiver = retired.receiver();
    let old_observer = old_sender.packet_observer();
    old_observer.register_observer(2).unwrap();
    assert!(old_observer.same_resource(&retired.sender().packet_observer()));
    negotiate(&alice, &bob);
    retired.stop_standard().unwrap();
    negotiate(&alice, &bob);
    assert!(retired.stopped());
    assert!(
        !alice
            .video_transceivers()
            .unwrap()
            .iter()
            .any(|value| value.same_resource(&retired))
    );
    let fresh_source = factory.create_video_source().unwrap();
    let fresh_track = factory
        .create_video_track("reused-endpoint-id", &fresh_source)
        .unwrap();
    let replacement = alice
        .add_video_transceiver(&fresh_track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let new_sender = replacement.sender();
    assert_eq!(
        old_sender.id(),
        new_sender.id(),
        "require an actual protocol-ID collision, not merely two distinct endpoints"
    );
    assert!(!retired.same_resource(&replacement));
    assert!(!old_sender.same_resource(&new_sender));
    assert!(!old_receiver.same_resource(&replacement.receiver()));
    let new_observer = new_sender.packet_observer();
    assert!(!old_observer.same_resource(&new_observer));
    new_observer.register_observer(2).unwrap();
    assert!(!old_sender.track().unwrap().enabled());
    assert!(new_sender.track().unwrap().enabled());
    alice.close().unwrap();
    bob.close().unwrap();
    old_observer.unregister_observer();
    new_observer.unregister_observer();
}
