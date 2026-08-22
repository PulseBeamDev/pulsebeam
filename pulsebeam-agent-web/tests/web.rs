use pulsebeam_agent_core::{ConnectionState, CoreConfig, MediaKind, TrackId, TransportGeneration};
use pulsebeam_agent_web::interop::{DataChannelConfig, PeerConfig};
use pulsebeam_agent_web::{GenerationEvent, WebParticipant, WebTransport};

#[test]
fn public_web_surface_keeps_generation_and_browser_effects_value_owned() {
    let participant = WebParticipant::new(CoreConfig::default(), PeerConfig::default())
        .expect("web participant should construct");
    assert_eq!(participant.core().state(), ConnectionState::Idle);

    let mut transport =
        WebTransport::new(PeerConfig::default()).expect("transport should construct");
    transport.register_channel(DataChannelConfig::reliable("v1/sys/signaling"));
    transport
        .connect(TransportGeneration::new(1))
        .expect("first generation should connect");
    assert_eq!(transport.generation(), Some(TransportGeneration::new(1)));

    let _ = (
        MediaKind::Audio,
        TrackId::from("track"),
        GenerationEvent::new(
            TransportGeneration::new(1),
            pulsebeam_agent_web::BrowserEvent::Connected,
        ),
    );
}
