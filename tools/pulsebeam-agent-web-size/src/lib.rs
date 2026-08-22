use pulsebeam_agent_web::interop::{DataChannelConfig, PeerConfig, SenderPreset};
use pulsebeam_agent_web::{SenderUpdateQueue, TransportGeneration, WebTransport};

#[cfg(feature = "production")]
use pulsebeam_agent_web::{CoreConfig, E2eeContext, E2eeEpoch, E2eeMasterKey, WebParticipant};

#[unsafe(no_mangle)]
pub extern "C" fn pulsebeam_agent_web_size_fixture() -> u32 {
    let config = PeerConfig::default().bounded();
    let mut transport = WebTransport::new(config.clone()).expect("bounded peer config is valid");
    transport.register_channel(DataChannelConfig::reliable("v1/sys/signaling"));
    let _ = transport.connect(TransportGeneration::new(1));

    #[cfg(feature = "production")]
    {
        let mut participant = WebParticipant::new(CoreConfig::default(), config.clone())
            .expect("participant is constructible");
        let _ = participant.register_ordered_publisher("size", "fixture", 1);
        participant.register_latest_publisher("latest");

        let key = E2eeMasterKey::new(1, [7; 32]);
        let epoch = E2eeEpoch::new([3; 16]).expect("fixture epoch is valid");
        let mut e2ee =
            E2eeContext::new(key, epoch, "fixture", "size").expect("fixture E2EE is valid");
        let frame = e2ee
            .encrypt_frame(&[])
            .expect("fixture encryption succeeds");
        let _ = e2ee
            .decrypt_frame(&frame)
            .expect("fixture decryption succeeds");
    }

    let mut updates = SenderUpdateQueue::new();
    updates.enqueue("base", SenderPreset::inactive());
    u32::try_from(config.video_slots + config.audio_slots + usize::from(!updates.is_empty()) + 1)
        .expect("fixture result fits in u32")
}
