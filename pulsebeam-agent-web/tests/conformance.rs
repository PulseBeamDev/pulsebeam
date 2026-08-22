use pulsebeam_agent_core::{CoreConfig, MonotonicTime, TransportGeneration};
use pulsebeam_agent_web::interop::{DataChannelConfig, PeerConfig, SIGNALING_LABEL, SenderPreset};
use pulsebeam_agent_web::{SenderUpdateQueue, WebParticipant, WebTransport};

#[test]
fn web_transport_keeps_browser_contract_value_owned() {
    let mut transport = WebTransport::new(PeerConfig::default()).expect("peer config is valid");
    transport.register_channel(DataChannelConfig::reliable(SIGNALING_LABEL));
    let generation = TransportGeneration::new(1);
    transport
        .connect(generation)
        .expect("first generation connects");
    transport
        .send(generation, SIGNALING_LABEL, vec![1, 2, 3])
        .expect("registered channel accepts payload");
    assert_eq!(
        transport.poll_sent(),
        Some((generation, SIGNALING_LABEL.to_owned(), vec![1, 2, 3]))
    );
    let mut updates = SenderUpdateQueue::new();
    updates.enqueue("sender-0", SenderPreset::inactive());
    assert!(!updates.is_empty());
    assert_eq!(
        updates.take_next().map(|(sender, _)| sender),
        Some(String::from("sender-0"))
    );
    assert!(updates.is_empty());
}

#[test]
fn web_participant_uses_core_for_initial_state() {
    let participant = WebParticipant::new(CoreConfig::default(), PeerConfig::default())
        .expect("participant is constructible without browser services");
    assert_eq!(
        participant.core().state(),
        pulsebeam_agent_core::ConnectionState::Idle
    );
    assert_eq!(participant.core().next_deadline(), None);
    assert_eq!(MonotonicTime::ZERO, MonotonicTime::from_millis(0));
}
