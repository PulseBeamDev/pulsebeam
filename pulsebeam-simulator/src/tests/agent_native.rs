use pulsebeam_agent_core::{
    ConnectionState, CoreConfig, CoreInput, MonotonicTime, TransportGeneration,
};
use pulsebeam_agent_native::media::{RtpPacket, RtpRecoveryBuffer};
use pulsebeam_agent_native::{AgentDriver, AgentError, NativeTransport};

#[tokio::test(flavor = "current_thread")]
async fn native_driver_keeps_core_lifecycle_generation_scoped() {
    let mut driver = AgentDriver::new(CoreConfig::default(), NativeTransport::None);
    driver
        .handle(MonotonicTime::ZERO, CoreInput::Start)
        .await
        .expect("native driver should execute the core connect effect");
    assert_eq!(driver.core().state(), ConnectionState::Connected);
    assert_eq!(driver.core().generation(), TransportGeneration::new(1));

    driver
        .handle(
            MonotonicTime::from_secs(1),
            CoreInput::TransportConnected {
                generation: TransportGeneration::new(1),
            },
        )
        .await
        .expect("duplicate transport-connected input is idempotent");
    assert!(matches!(
        driver.dispatch_datagram(TransportGeneration::INITIAL, vec![1]),
        Err(AgentError::Core(
            pulsebeam_agent_core::CoreError::StaleGeneration { .. }
        ))
    ));
}

#[test]
fn native_recovery_never_delivers_a_future_packet_twice() {
    let mut recovery = RtpRecoveryBuffer::new(8);
    assert_eq!(recovery.accept(packet(10)), vec![result(10)]);
    assert!(recovery.accept(packet(12)).is_empty());
    assert_eq!(recovery.missing(), vec![11]);
    assert_eq!(recovery.recover(packet(11)), vec![result(11), result(12)]);
    assert!(recovery.recover(packet(12)).is_empty());
}

fn packet(sequence: u16) -> RtpPacket {
    RtpPacket {
        mid: String::from("video"),
        sequence,
        timestamp: 100,
        marker: sequence == 12,
        payload: vec![sequence as u8],
    }
}

fn result(sequence: u16) -> pulsebeam_agent_native::media::RecoveryResult {
    pulsebeam_agent_native::media::RecoveryResult {
        packet: packet(sequence),
        missing: Vec::new(),
    }
}
