use super::*;
use crate::{
    DataRate, DataSize, Environment, FieldTrials, PeerConnectionFactory, PeerErrorKind, Timestamp,
};
use std::{cell::Cell, sync::atomic::AtomicUsize};
#[derive(Clone, Debug, PartialEq)]
enum Input {
    NetworkAvailability(NetworkAvailability),
    NetworkRouteChange(NetworkRouteChange),
    ProcessInterval(ProcessInterval),
    RemoteBitrateReport(RemoteBitrateReport),
    RoundTripTimeUpdate(RoundTripTimeUpdate),
    SentPacket(SentPacket),
    ReceivedPacket(ReceivedPacket),
    StreamsConfig(StreamsConfig),
    TargetRateConstraints(TargetRateConstraints),
    TransportLossReport(TransportLossReport),
    TransportPacketsFeedback(TransportPacketsFeedback),
    NetworkStateEstimate(NetworkStateEstimate),
}
#[derive(Default)]
struct Observations {
    calls: Mutex<Vec<Input>>,
    config: Mutex<Option<(TargetRateConstraints, StreamsConfig, TimeDelta)>>,
    environment: Mutex<Option<NetworkControllerEnvironment>>,
    factory_drops: AtomicUsize,
    product_drops: AtomicUsize,
    intervals: AtomicUsize,
    ecn_calls: AtomicUsize,
}
#[derive(Clone, Copy)]
enum Behavior {
    Good,
    CallerRetainsFactory,
    PanicFactory,
    PanicInterval,
    NegativeInterval,
    PanicUpdate,
    PanicEcn,
    InvalidUnits,
    InfiniteRoundTripTime,
    ZeroProbeSize,
    WideProbeTimestampSpan,
    PerProbeOvershoot,
    DropPanics,
}
struct Factory {
    observations: Arc<Observations>,
    behavior: Behavior,
    counter: Cell<u32>,
}
impl Drop for Factory {
    fn drop(&mut self) {
        self.observations
            .factory_drops
            .fetch_add(1, Ordering::Relaxed);
        if matches!(self.behavior, Behavior::DropPanics) {
            panic!("factory drop");
        }
    }
}
impl NetworkControllerFactoryProvider for Factory {
    fn create(&mut self, config: NetworkControllerConfig) -> Box<dyn NetworkController> {
        if matches!(self.behavior, Behavior::PanicFactory) {
            panic!("factory create");
        }
        self.counter.set(self.counter.get() + 1);
        *self.observations.config.lock().unwrap() = Some((
            config.constraints,
            config.stream_based_config,
            config.default_pacing_time_window,
        ));
        *self.observations.environment.lock().unwrap() = Some(config.environment);
        Box::new(Controller {
            observations: self.observations.clone(),
            behavior: self.behavior,
            counter: Cell::new(0),
        })
    }
    fn process_interval(&mut self) -> TimeDelta {
        self.observations.intervals.fetch_add(1, Ordering::Relaxed);
        match self.behavior {
            Behavior::PanicInterval => panic!("interval"),
            Behavior::NegativeInterval => TimeDelta::from_native_microsecond_bits(-1),
            _ => TimeDelta::from_native_microsecond_bits(25_000),
        }
    }
}
struct Controller {
    observations: Arc<Observations>,
    behavior: Behavior,
    counter: Cell<u32>,
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.observations
            .product_drops
            .fetch_add(1, Ordering::Relaxed);
        if matches!(self.behavior, Behavior::DropPanics) {
            panic!("product drop");
        }
    }
}
fn fixed_update() -> NetworkControlUpdate {
    let at_time = Timestamp::from_native_microsecond_bits(1000);
    NetworkControlUpdate {
        congestion_window: Some(DataSize::Bytes(0)),
        pacer_config: Some(
            PacerConfig::create(
                at_time,
                DataRate::BitsPerSecond(2_000_000),
                DataRate::BitsPerSecond(1_000_000),
                TimeDelta::zero(),
            )
            .unwrap(),
        ),
        probe_cluster_configs: vec![ProbeClusterConfig {
            at_time,
            target_data_rate: DataRate::BitsPerSecond(3_000_000),
            target_duration: TimeDelta::from_native_microsecond_bits(3000),
            min_probe_delta: TimeDelta::from_native_microsecond_bits(2000),
            target_probe_count: 3,
            id: 17,
        }],
        target_rate: Some(TargetTransferRate {
            at_time,
            network_estimate: NetworkEstimate {
                at_time,
                bandwidth: DataRate::BitsPerSecond(1_000_000),
                round_trip_time: TimeDelta::from_native_microsecond_bits(250),
                loss_rate_ratio: 0.25,
            },
            target_rate: DataRate::BitsPerSecond(1_500_000),
            cwnd_reduce_ratio: 0.125,
        }),
    }
}
impl Controller {
    fn record(&mut self, value: Input) -> NetworkControlUpdate {
        assert_eq!(
            self.observations.factory_drops.load(Ordering::Relaxed),
            usize::from(!matches!(self.behavior, Behavior::CallerRetainsFactory))
        );
        self.counter.set(self.counter.get() + 1);
        self.observations.calls.lock().unwrap().push(value);
        if matches!(self.behavior, Behavior::PanicUpdate) {
            panic!("update");
        }
        let mut update = fixed_update();
        if matches!(self.behavior, Behavior::InvalidUnits) {
            update.congestion_window = Some(DataSize::Bytes(-1));
        }
        if matches!(self.behavior, Behavior::InfiniteRoundTripTime) {
            update
                .target_rate
                .as_mut()
                .unwrap()
                .network_estimate
                .round_trip_time = TimeDelta::plus_infinity();
        }
        match self.behavior {
            Behavior::ZeroProbeSize => {
                let probe = &mut update.probe_cluster_configs[0];
                probe.target_data_rate = DataRate::BitsPerSecond(1);
                probe.min_probe_delta = TimeDelta::from_native_microsecond_bits(1);
            }
            Behavior::WideProbeTimestampSpan => {
                update.probe_cluster_configs[0].at_time =
                    Timestamp::from_native_microsecond_bits(if self.counter.get() == 1 {
                        i64::MAX - 1
                    } else {
                        i64::MIN + 1
                    });
            }
            Behavior::PerProbeOvershoot => {
                let probe = &mut update.probe_cluster_configs[0];
                probe.target_data_rate = DataRate::BitsPerSecond(8000);
                probe.min_probe_delta = TimeDelta::from_native_microsecond_bits(1000);
                probe.target_probe_count = i32::MAX / 2;
            }
            _ => {}
        }
        update
    }
}
impl NetworkController for Controller {
    fn on_network_availability(&mut self, value: NetworkAvailability) -> NetworkControlUpdate {
        self.record(Input::NetworkAvailability(value))
    }
    fn on_network_route_change(&mut self, value: NetworkRouteChange) -> NetworkControlUpdate {
        self.record(Input::NetworkRouteChange(value))
    }
    fn on_process_interval(&mut self, value: ProcessInterval) -> NetworkControlUpdate {
        self.record(Input::ProcessInterval(value))
    }
    fn on_remote_bitrate_report(&mut self, value: RemoteBitrateReport) -> NetworkControlUpdate {
        self.record(Input::RemoteBitrateReport(value))
    }
    fn on_round_trip_time_update(&mut self, value: RoundTripTimeUpdate) -> NetworkControlUpdate {
        self.record(Input::RoundTripTimeUpdate(value))
    }
    fn on_sent_packet(&mut self, value: SentPacket) -> NetworkControlUpdate {
        self.record(Input::SentPacket(value))
    }
    fn on_received_packet(&mut self, value: ReceivedPacket) -> NetworkControlUpdate {
        self.record(Input::ReceivedPacket(value))
    }
    fn on_streams_config(&mut self, value: StreamsConfig) -> NetworkControlUpdate {
        self.record(Input::StreamsConfig(value))
    }
    fn on_target_rate_constraints(&mut self, value: TargetRateConstraints) -> NetworkControlUpdate {
        self.record(Input::TargetRateConstraints(value))
    }
    fn on_transport_loss_report(&mut self, value: TransportLossReport) -> NetworkControlUpdate {
        self.record(Input::TransportLossReport(value))
    }
    fn on_transport_packets_feedback(
        &mut self,
        value: TransportPacketsFeedback,
    ) -> NetworkControlUpdate {
        self.record(Input::TransportPacketsFeedback(value))
    }
    fn on_network_state_estimate(&mut self, value: NetworkStateEstimate) -> NetworkControlUpdate {
        self.record(Input::NetworkStateEstimate(value))
    }
    fn supports_ecn_adaptation(&mut self) -> bool {
        self.observations.ecn_calls.fetch_add(1, Ordering::Relaxed);
        if matches!(self.behavior, Behavior::PanicEcn) {
            panic!("ecn");
        }
        true
    }
}
fn registration(observations: &Arc<Observations>, behavior: Behavior) -> NetworkControllerFactory {
    NetworkControllerFactory::from_provider(Factory {
        observations: observations.clone(),
        behavior,
        counter: Cell::new(0),
    })
}
fn invoke(
    environment: &Environment,
    observations: &Arc<Observations>,
    behavior: Behavior,
    messages: &[ffi::FfiNcMessage],
    kinds: &[u8],
) -> (Vec<NetworkControlUpdate>, Option<NetworkControllerFailure>) {
    let factory = registration(observations, behavior);
    let observer = factory.0.observed_failure.clone();
    let replies = ffi::network_controller_native_probe(
        environment.native(),
        Box::new(RustNetworkControllerFactory(Some(factory), None)),
        ffi::FfiNcConfig {
            constraints: TargetRateConstraints::default().native(),
            stream_based_config: StreamsConfig::default().native(),
            default_pacing_time_window: PacerConfig::DEFAULT_TIME_WINDOW.native_microsecond_bits(),
        },
        messages,
        kinds,
    );
    assert!(replies.iter().all(|reply| !reply.failed));
    (
        replies
            .into_iter()
            .map(NetworkControlUpdate::from_native)
            .collect(),
        decode_failure(observer.load(Ordering::Acquire)),
    )
}

fn sample_streams_config() -> StreamsConfig {
    StreamsConfig {
        at_time: Timestamp::from_native_microsecond_bits(-17),
        requests_alr_probing: Some(false),
        enable_repeated_initial_probing: false,
        pacing_factor: Some(0.375),
        min_total_allocated_bitrate: Some(DataRate::NegativeInfinity),
        max_padding_rate: Some(DataRate::NegativeInfinity),
        max_total_allocated_bitrate: Some(DataRate::NegativeInfinity),
    }
}

fn sample_target_rate_constraints() -> TargetRateConstraints {
    TargetRateConstraints {
        at_time: Timestamp::from_native_microsecond_bits(-17),
        min_data_rate: Some(DataRate::NegativeInfinity),
        max_data_rate: Some(DataRate::NegativeInfinity),
        starting_rate: Some(DataRate::NegativeInfinity),
    }
}

fn sample_network_availability() -> NetworkAvailability {
    NetworkAvailability {
        at_time: Timestamp::from_native_microsecond_bits(-17),
        network_available: false,
    }
}

fn sample_network_route_change() -> NetworkRouteChange {
    NetworkRouteChange {
        at_time: Timestamp::from_native_microsecond_bits(-17),
        constraints: sample_target_rate_constraints(),
        restart_bwe: false,
    }
}

fn sample_paced_packet_info() -> PacedPacketInfo {
    PacedPacketInfo {
        send_bitrate: DataRate::NegativeInfinity,
        probe_cluster_id: i32::MIN,
        probe_cluster_min_probes: i32::MIN,
        probe_cluster_min_bytes: i32::MIN,
        probe_cluster_bytes_sent: i32::MIN,
    }
}

fn sample_sent_packet() -> SentPacket {
    SentPacket {
        send_time: Timestamp::from_native_microsecond_bits(-17),
        size: DataSize::PositiveInfinity,
        prior_unacked_data: DataSize::PositiveInfinity,
        pacing_info: sample_paced_packet_info(),
        audio: false,
        sequence_number: i64::MIN,
        data_in_flight: DataSize::PositiveInfinity,
    }
}

fn sample_received_packet() -> ReceivedPacket {
    ReceivedPacket {
        send_time: Timestamp::from_native_microsecond_bits(-17),
        receive_time: Timestamp::from_native_microsecond_bits(-17),
        size: DataSize::PositiveInfinity,
    }
}

fn sample_remote_bitrate_report() -> RemoteBitrateReport {
    RemoteBitrateReport {
        receive_time: Timestamp::from_native_microsecond_bits(-17),
        bandwidth: DataRate::NegativeInfinity,
    }
}

fn sample_round_trip_time_update() -> RoundTripTimeUpdate {
    RoundTripTimeUpdate {
        receive_time: Timestamp::from_native_microsecond_bits(-17),
        round_trip_time: TimeDelta::minus_infinity(),
        smoothed: false,
    }
}

fn sample_transport_loss_report() -> TransportLossReport {
    TransportLossReport {
        receive_time: Timestamp::from_native_microsecond_bits(-17),
        start_time: Timestamp::from_native_microsecond_bits(-17),
        end_time: Timestamp::from_native_microsecond_bits(-17),
        packets_lost_delta: u64::MAX,
        packets_received_delta: u64::MAX,
    }
}

fn sample_rtp_packet_info() -> RtpPacketInfo {
    RtpPacketInfo {
        ssrc: u32::MAX,
        rtp_sequence_number: u16::MAX,
        is_retransmission: false,
    }
}

fn sample_packet_result() -> PacketResult {
    PacketResult {
        sent_packet: sample_sent_packet(),
        receive_time: Timestamp::from_native_microsecond_bits(-17),
        arrival_time_offset: Some(TimeDelta::minus_infinity()),
        ecn: EcnMarking::Unknown(255),
        sent_with_ect1: false,
        reported_lost_for_the_first_time: false,
        reported_recovered_for_the_first_time: false,
        rtp_packet_info: Some(sample_rtp_packet_info()),
    }
}

fn sample_transport_packets_feedback() -> TransportPacketsFeedback {
    TransportPacketsFeedback {
        feedback_time: Timestamp::from_native_microsecond_bits(-17),
        data_in_flight: DataSize::PositiveInfinity,
        transport_supports_ecn: false,
        packet_feedbacks: vec![sample_packet_result(), Default::default()],
        sendless_arrival_times: vec![
            Timestamp::from_native_microsecond_bits(-17),
            Timestamp::zero(),
        ],
    }
}

fn sample_process_interval() -> ProcessInterval {
    ProcessInterval {
        at_time: Timestamp::from_native_microsecond_bits(-17),
        pacer_queue: Some(DataSize::PositiveInfinity),
    }
}

fn sample_network_state_estimate() -> NetworkStateEstimate {
    NetworkStateEstimate {
        confidence: 0.375,
        update_time: Timestamp::from_native_microsecond_bits(-17),
        last_receive_time: Timestamp::from_native_microsecond_bits(-17),
        last_send_time: Timestamp::from_native_microsecond_bits(-17),
        link_capacity: DataRate::NegativeInfinity,
        link_capacity_lower: DataRate::NegativeInfinity,
        link_capacity_upper: DataRate::NegativeInfinity,
        pre_link_buffer_delay: TimeDelta::minus_infinity(),
        post_link_buffer_delay: TimeDelta::minus_infinity(),
        propagation_delay: TimeDelta::minus_infinity(),
        time_delta: TimeDelta::minus_infinity(),
        last_feed_time: Timestamp::from_native_microsecond_bits(-17),
        cross_delay_rate: 0.375,
        spike_delay_rate: 0.375,
        link_capacity_std_dev: DataRate::NegativeInfinity,
        link_capacity_min: DataRate::NegativeInfinity,
        cross_traffic_ratio: 0.375,
    }
}

#[test]
fn native_full_graph_ownership_and_presence() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<NetworkControllerFactory>();
    send_sync::<NetworkControllerEnvironment>();
    send_sync::<NetworkControlUpdate>();
    let environment = Environment::builder().build().unwrap();
    let observations = Arc::new(Observations::default());
    let inputs = vec![
        Input::NetworkAvailability(sample_network_availability()),
        Input::NetworkRouteChange(sample_network_route_change()),
        Input::ProcessInterval(sample_process_interval()),
        Input::RemoteBitrateReport(sample_remote_bitrate_report()),
        Input::RoundTripTimeUpdate(sample_round_trip_time_update()),
        Input::SentPacket(sample_sent_packet()),
        Input::ReceivedPacket(sample_received_packet()),
        Input::StreamsConfig(sample_streams_config()),
        Input::TargetRateConstraints(sample_target_rate_constraints()),
        Input::TransportLossReport(sample_transport_loss_report()),
        Input::TransportPacketsFeedback(sample_transport_packets_feedback()),
        Input::NetworkStateEstimate(sample_network_state_estimate()),
    ];
    let messages: Vec<_> = inputs
        .iter()
        .map(|input| match input {
            Input::NetworkAvailability(value) => ffi::FfiNcMessage {
                network_availability: value.native(),
                ..Default::default()
            },
            Input::NetworkRouteChange(value) => ffi::FfiNcMessage {
                network_route_change: value.native(),
                ..Default::default()
            },
            Input::ProcessInterval(value) => ffi::FfiNcMessage {
                process_interval: value.native(),
                ..Default::default()
            },
            Input::RemoteBitrateReport(value) => ffi::FfiNcMessage {
                remote_bitrate_report: value.native(),
                ..Default::default()
            },
            Input::RoundTripTimeUpdate(value) => ffi::FfiNcMessage {
                round_trip_time_update: value.native(),
                ..Default::default()
            },
            Input::SentPacket(value) => ffi::FfiNcMessage {
                sent_packet: value.native(),
                ..Default::default()
            },
            Input::ReceivedPacket(value) => ffi::FfiNcMessage {
                received_packet: value.native(),
                ..Default::default()
            },
            Input::StreamsConfig(value) => ffi::FfiNcMessage {
                streams_config: value.native(),
                ..Default::default()
            },
            Input::TargetRateConstraints(value) => ffi::FfiNcMessage {
                target_rate_constraints: value.native(),
                ..Default::default()
            },
            Input::TransportLossReport(value) => ffi::FfiNcMessage {
                transport_loss_report: value.native(),
                ..Default::default()
            },
            Input::TransportPacketsFeedback(value) => ffi::FfiNcMessage {
                transport_packets_feedback: value.native(),
                ..Default::default()
            },
            Input::NetworkStateEstimate(value) => ffi::FfiNcMessage {
                network_state_estimate: value.native(),
                ..Default::default()
            },
        })
        .collect();
    let kinds: Vec<u8> = (0..12).collect();
    let (replies, failure) = invoke(
        &environment,
        &observations,
        Behavior::Good,
        &messages,
        &kinds,
    );
    drop(messages);
    assert_eq!(*observations.calls.lock().unwrap(), inputs);
    assert_eq!(failure, None);
    for reply in &replies {
        assert_eq!(reply, &fixed_update());
    }
    assert_eq!(observations.intervals.load(Ordering::Relaxed), 1);
    assert_eq!(observations.product_drops.load(Ordering::Relaxed), 1);
    assert_eq!(
        *observations.config.lock().unwrap(),
        Some((
            TargetRateConstraints::default(),
            StreamsConfig::default(),
            PacerConfig::DEFAULT_TIME_WINDOW
        ))
    );
    let trials = environment.field_trials();
    drop(environment);
    let retained = observations.environment.lock().unwrap().take().unwrap();
    assert_eq!(
        retained
            .field_trials()
            .lookup("WebRTC-Pacer-DynamicPadding"),
        trials.lookup("WebRTC-Pacer-DynamicPadding")
    );
    assert!(retained.timestamp().is_finite());
}
#[test]
fn native_factory_product_interval_and_drop_panics_are_contained() {
    let environment = Environment::builder().build().unwrap();
    for (behavior, expected) in [
        (Behavior::PanicFactory, NetworkControllerFailure::Panicked),
        (Behavior::PanicInterval, NetworkControllerFailure::Panicked),
        (
            Behavior::NegativeInterval,
            NetworkControllerFailure::InvalidProcessInterval,
        ),
        (Behavior::PanicUpdate, NetworkControllerFailure::Panicked),
        (
            Behavior::InvalidUnits,
            NetworkControllerFailure::InvalidUnits,
        ),
    ] {
        let observations = Arc::new(Observations::default());
        let (replies, failure) = invoke(
            &environment,
            &observations,
            behavior,
            &[Default::default(), Default::default()],
            &[0, 2],
        );
        assert_eq!(failure, Some(expected));
        for reply in replies {
            assert_eq!(
                reply.target_rate.unwrap().target_rate,
                DataRate::BitsPerSecond(0)
            );
            assert!(reply.pacer_config.is_none());
        }
        assert_eq!(observations.factory_drops.load(Ordering::Relaxed), 1);
        assert_eq!(
            observations.intervals.load(Ordering::Relaxed),
            usize::from(!matches!(behavior, Behavior::PanicFactory))
        );
        assert_eq!(
            observations.product_drops.load(Ordering::Relaxed),
            if matches!(behavior, Behavior::PanicFactory) {
                0
            } else {
                1
            }
        );
    }
    let observations = Arc::new(Observations::default());
    let (_, failure) = invoke(
        &environment,
        &observations,
        Behavior::DropPanics,
        &[Default::default()],
        &[0],
    );
    assert_eq!(failure, None);
    assert_eq!(observations.product_drops.load(Ordering::Relaxed), 1);
}
#[test]
fn native_adapter_preserves_wide_probe_timestamps_without_shadow_history() {
    let environment = Environment::builder().build().unwrap();
    let observations = Arc::new(Observations::default());
    let (replies, failure) = invoke(
        &environment,
        &observations,
        Behavior::WideProbeTimestampSpan,
        &[Default::default(), Default::default(), Default::default()],
        &[0, 2, 0],
    );
    // This driver observes adapter conversion only, not private pacer arithmetic.
    assert_eq!(failure, None);
    assert_eq!(replies.len(), 3);
    for (reply, time) in replies
        .iter()
        .zip([i64::MAX - 1, i64::MIN + 1, i64::MIN + 1])
    {
        let mut expected = fixed_update();
        expected.probe_cluster_configs[0].at_time = Timestamp::from_native_microsecond_bits(time);
        assert_eq!(reply, &expected);
    }
    assert_eq!(observations.calls.lock().unwrap().len(), 3);
    assert_eq!(observations.product_drops.load(Ordering::Relaxed), 1);
}
#[test]
fn native_adapter_forwards_representable_updates_without_arithmetic_budgets() {
    let environment = Environment::builder().build().unwrap();
    for behavior in [
        Behavior::InfiniteRoundTripTime,
        Behavior::ZeroProbeSize,
        Behavior::PerProbeOvershoot,
    ] {
        let observations = Arc::new(Observations::default());
        let (replies, failure) = invoke(
            &environment,
            &observations,
            behavior,
            &[Default::default()],
            &[0],
        );
        let mut expected = fixed_update();
        match behavior {
            Behavior::InfiniteRoundTripTime => {
                expected
                    .target_rate
                    .as_mut()
                    .unwrap()
                    .network_estimate
                    .round_trip_time = TimeDelta::plus_infinity();
            }
            Behavior::ZeroProbeSize => {
                expected.probe_cluster_configs[0].target_data_rate = DataRate::BitsPerSecond(1);
                expected.probe_cluster_configs[0].min_probe_delta =
                    TimeDelta::from_native_microsecond_bits(1);
            }
            Behavior::PerProbeOvershoot => {
                expected.probe_cluster_configs[0].target_data_rate = DataRate::BitsPerSecond(8000);
                expected.probe_cluster_configs[0].min_probe_delta =
                    TimeDelta::from_native_microsecond_bits(1000);
                expected.probe_cluster_configs[0].target_probe_count = i32::MAX / 2;
            }
            _ => unreachable!(),
        }
        assert_eq!(failure, None);
        assert_eq!(replies, vec![expected]);
    }
}

#[test]
fn explicit_factory_trial_gate() {
    let observations = Arc::new(Observations::default());
    let factory = registration(&observations, Behavior::Good);
    let error = PeerConnectionFactory::builder()
        .network_controller_factory(&factory)
        .build()
        .err()
        .expect("disabled root trial");
    assert_eq!(error.kind, PeerErrorKind::InvalidParameter);
    assert!(!factory.has_failed());
    let enabled = Environment::builder()
        .field_trials(
            FieldTrials::parse("WebRTC-Bwe-InjectedCongestionController/Enabled/").unwrap(),
        )
        .build()
        .unwrap();
    assert!(ffi::network_controller_injection_enabled(enabled.native()));
}
#[test]
fn packet_feedback_helpers_match_native_order_and_marking() {
    let allocation_limits = BitrateAllocationLimits::default();
    assert_eq!(
        BitrateAllocationLimits::from_native(allocation_limits.native()),
        allocation_limits
    );
    let sent = SentPacket {
        send_time: Timestamp::zero(),
        ..Default::default()
    };
    let received = PacketResult {
        sent_packet: sent.clone(),
        receive_time: Timestamp::zero(),
        ecn: EcnMarking::CongestionExperienced,
        ..Default::default()
    };
    let lost = PacketResult {
        sent_packet: sent.clone(),
        ..Default::default()
    };
    let mut earlier = received.clone();
    earlier.sent_packet.sequence_number = -1;
    let feedback = TransportPacketsFeedback {
        packet_feedbacks: vec![received.clone(), lost.clone(), earlier.clone()],
        ..Default::default()
    };
    assert_eq!(feedback.lost_with_send_info(), vec![lost]);
    assert_eq!(
        feedback.received_with_send_info(),
        vec![received.clone(), earlier.clone()]
    );
    assert_eq!(feedback.sorted_by_receive_time(), vec![earlier, received]);
    assert!(feedback.has_packet_with_ecn_ce());
    assert_eq!(feedback.packets_with_feedback(), feedback.packet_feedbacks);
    let config = PacerConfig::create(
        Timestamp::zero(),
        DataRate::BitsPerSecond(8000),
        DataRate::BitsPerSecond(4000),
        TimeDelta::zero(),
    )
    .unwrap();
    assert_eq!(config.data_rate(), Ok(DataRate::BitsPerSecond(8000)));
    assert_eq!(config.pad_rate(), Ok(DataRate::BitsPerSecond(4000)));
    assert_eq!(
        PacerConfig::create(
            Timestamp::zero(),
            DataRate::BitsPerSecond(i64::MAX),
            DataRate::BitsPerSecond(0),
            TimeDelta::zero()
        ),
        Err(NetworkControlValueError::OutOfRange)
    );
    assert!(!NetworkControlUpdate::default().has_updates());
    assert!(
        NetworkControlUpdate {
            congestion_window: Some(DataSize::Bytes(0)),
            ..Default::default()
        }
        .has_updates()
    );
}

#[test]
fn ecn_query_panics_are_contained_without_synthetic_updates() {
    let environment = Environment::builder().build().unwrap();
    for behavior in [Behavior::CallerRetainsFactory, Behavior::PanicEcn] {
        let observations = Arc::new(Observations::default());
        let factory = registration(&observations, behavior);
        let source = RustNetworkControllerFactory(Some(factory.clone()), Some(environment.clone()));
        let product = network_controller_create(
            &source,
            ffi::clone_environment(environment.native()),
            ffi::FfiNcConfig {
                constraints: TargetRateConstraints::default().native(),
                stream_based_config: StreamsConfig::default().native(),
                default_pacing_time_window: PacerConfig::DEFAULT_TIME_WINDOW
                    .native_microsecond_bits(),
            },
        );
        let panics = matches!(behavior, Behavior::PanicEcn);
        assert_eq!(network_controller_ecn(&product), !panics);
        assert_eq!(network_controller_ecn(&product), !panics);
        assert_eq!(
            observations.ecn_calls.load(Ordering::Relaxed),
            if panics { 1 } else { 2 }
        );
        let update = network_controller_update(
            &product,
            0,
            ffi::FfiNcMessage {
                network_availability: sample_network_availability().native(),
                ..Default::default()
            },
        );
        assert_eq!(update.failed, panics);
        assert_eq!(
            observations.calls.lock().unwrap().len(),
            usize::from(!panics)
        );
        assert_eq!(
            factory.observed_failure(),
            panics.then_some(NetworkControllerFailure::Panicked)
        );
    }
}

#[test]
fn test_replay_passes_explicit_initial_config_and_preserves_callback_order() {
    let observations = Arc::new(Observations::default());
    let factory = registration(&observations, Behavior::CallerRetainsFactory);
    let config = NetworkControllerReplayConfig {
        constraints: TargetRateConstraints {
            at_time: Timestamp::from_native_microsecond_bits(123),
            min_data_rate: Some(DataRate::BitsPerSecond(42_000)),
            max_data_rate: Some(DataRate::BitsPerSecond(2_000_000)),
            starting_rate: Some(DataRate::BitsPerSecond(900_000)),
        },
        stream_based_config: StreamsConfig {
            at_time: Timestamp::from_native_microsecond_bits(124),
            pacing_factor: Some(1.25),
            ..Default::default()
        },
        default_pacing_time_window: TimeDelta::from_native_microsecond_bits(17_000),
    };
    let events = vec![
        NetworkControllerReplayEvent::NetworkAvailability(sample_network_availability()),
        NetworkControllerReplayEvent::SentPacket(sample_sent_packet()),
        NetworkControllerReplayEvent::TransportPacketsFeedback(sample_transport_packets_feedback()),
        NetworkControllerReplayEvent::ProcessInterval(sample_process_interval()),
    ];
    let replies = network_controller_replay_for_test(
        &Environment::builder().build().unwrap(),
        &factory,
        config.clone(),
        &events,
    )
    .unwrap();
    assert_eq!(replies, vec![fixed_update(); events.len()]);
    assert_eq!(
        *observations.config.lock().unwrap(),
        Some((
            config.constraints,
            config.stream_based_config,
            config.default_pacing_time_window,
        ))
    );
    assert_eq!(
        observations.calls.lock().unwrap().as_slice(),
        &[
            Input::NetworkAvailability(sample_network_availability()),
            Input::SentPacket(sample_sent_packet()),
            Input::TransportPacketsFeedback(sample_transport_packets_feedback()),
            Input::ProcessInterval(sample_process_interval()),
        ]
    );
}

#[test]
fn test_replay_rejects_overlong_input_and_uses_builtin_scream_factory() {
    let environment = Environment::builder()
        .field_trials(FieldTrials::parse("WebRTC-Bwe-ScreamV2/mode:always/").unwrap())
        .build()
        .unwrap();
    let factory = NetworkControllerFactory::goog_cc(GoogCcFactoryConfig::new(false));
    let config = NetworkControllerReplayConfig {
        constraints: TargetRateConstraints {
            at_time: Timestamp::zero(),
            min_data_rate: Some(DataRate::BitsPerSecond(30_000)),
            max_data_rate: Some(DataRate::BitsPerSecond(3_000_000)),
            starting_rate: Some(DataRate::BitsPerSecond(1_000_000)),
        },
        stream_based_config: StreamsConfig::default(),
        default_pacing_time_window: TimeDelta::from_native_microsecond_bits(5_000),
    };
    assert_eq!(
        network_controller_replay_for_test(
            &environment,
            &factory,
            config.clone(),
            &vec![NetworkControllerReplayEvent::ProcessInterval(ProcessInterval::default()); 4097],
        ),
        Err(NetworkControllerReplayError::TooManyEvents)
    );
    assert_eq!(
        network_controller_replay_for_test(
            &environment,
            &factory,
            config.clone(),
            &[NetworkControllerReplayEvent::SentPacket(SentPacket {
                size: DataSize::Bytes(-1),
                ..Default::default()
            })],
        ),
        Err(NetworkControllerReplayError::InvalidNativeInput)
    );
    let events = vec![
        NetworkControllerReplayEvent::NetworkAvailability(NetworkAvailability {
            at_time: Timestamp::zero(),
            network_available: true,
        }),
        NetworkControllerReplayEvent::NetworkRouteChange(NetworkRouteChange {
            at_time: Timestamp::zero(),
            constraints: config.constraints.clone(),
            restart_bwe: true,
        }),
        NetworkControllerReplayEvent::SentPacket(SentPacket {
            send_time: Timestamp::from_native_microsecond_bits(10_000),
            size: DataSize::Bytes(1200),
            prior_unacked_data: DataSize::Bytes(0),
            sequence_number: 1,
            data_in_flight: DataSize::Bytes(1200),
            ..Default::default()
        }),
        NetworkControllerReplayEvent::TransportPacketsFeedback(TransportPacketsFeedback {
            feedback_time: Timestamp::from_native_microsecond_bits(30_000),
            data_in_flight: DataSize::Bytes(0),
            packet_feedbacks: vec![PacketResult {
                sent_packet: SentPacket {
                    send_time: Timestamp::from_native_microsecond_bits(10_000),
                    size: DataSize::Bytes(1200),
                    sequence_number: 1,
                    ..Default::default()
                },
                receive_time: Timestamp::from_native_microsecond_bits(30_000),
                ..Default::default()
            }],
            ..Default::default()
        }),
        NetworkControllerReplayEvent::ProcessInterval(ProcessInterval {
            at_time: Timestamp::from_native_microsecond_bits(40_000),
            pacer_queue: Some(DataSize::Bytes(0)),
        }),
    ];
    let updates =
        network_controller_replay_for_test(&environment, &factory, config, &events).unwrap();
    assert_eq!(updates.len(), events.len());
    assert!(updates.iter().any(NetworkControlUpdate::has_updates));
    assert!(
        updates
            .iter()
            .filter_map(|update| update.target_rate.as_ref())
            .any(|rate| matches!(rate.target_rate, DataRate::BitsPerSecond(value) if value > 0))
    );
}

#[test]
fn controlled_replay_advances_the_selected_manual_clock_at_construction_and_callbacks() {
    use crate::ManualClock;
    use std::time::Duration;

    struct ClockFactory(Arc<Mutex<Vec<i64>>>);
    impl NetworkControllerFactoryProvider for ClockFactory {
        fn create(&mut self, config: NetworkControllerConfig) -> Box<dyn NetworkController> {
            let clock = config.environment.environment().clock().unwrap();
            let now = clock.current_time().microseconds_or(-1);
            self.0.lock().unwrap().push(now);
            Box::new(ClockController(self.0.clone(), clock))
        }
        fn process_interval(&mut self) -> TimeDelta {
            TimeDelta::from_native_microsecond_bits(1_000)
        }
    }
    struct ClockController(Arc<Mutex<Vec<i64>>>, crate::Clock);
    impl ClockController {
        fn record(&self) -> NetworkControlUpdate {
            self.0
                .lock()
                .unwrap()
                .push(self.1.current_time().microseconds_or(-1));
            NetworkControlUpdate::default()
        }
    }
    impl NetworkController for ClockController {
        fn on_network_availability(&mut self, _: NetworkAvailability) -> NetworkControlUpdate {
            self.record()
        }
        fn on_network_route_change(&mut self, _: NetworkRouteChange) -> NetworkControlUpdate {
            self.record()
        }
        fn on_process_interval(&mut self, _: ProcessInterval) -> NetworkControlUpdate {
            self.record()
        }
        fn on_remote_bitrate_report(&mut self, _: RemoteBitrateReport) -> NetworkControlUpdate {
            self.record()
        }
        fn on_round_trip_time_update(&mut self, _: RoundTripTimeUpdate) -> NetworkControlUpdate {
            self.record()
        }
        fn on_sent_packet(&mut self, _: SentPacket) -> NetworkControlUpdate {
            self.record()
        }
        fn on_received_packet(&mut self, _: ReceivedPacket) -> NetworkControlUpdate {
            self.record()
        }
        fn on_streams_config(&mut self, _: StreamsConfig) -> NetworkControlUpdate {
            self.record()
        }
        fn on_target_rate_constraints(&mut self, _: TargetRateConstraints) -> NetworkControlUpdate {
            self.record()
        }
        fn on_transport_loss_report(&mut self, _: TransportLossReport) -> NetworkControlUpdate {
            self.record()
        }
        fn on_transport_packets_feedback(
            &mut self,
            _: TransportPacketsFeedback,
        ) -> NetworkControlUpdate {
            self.record()
        }
        fn on_network_state_estimate(&mut self, _: NetworkStateEstimate) -> NetworkControlUpdate {
            self.record()
        }
    }

    let times = Arc::new(Mutex::new(Vec::new()));
    let manual_clock = ManualClock::new(Duration::ZERO).unwrap();
    let environment = Environment::builder().clock(&manual_clock).build().unwrap();
    let factory = NetworkControllerFactory::from_provider(ClockFactory(times.clone()));
    let config = NetworkControllerReplayConfig {
        constraints: TargetRateConstraints {
            at_time: Timestamp::from_native_microsecond_bits(100_000),
            ..Default::default()
        },
        stream_based_config: StreamsConfig::default(),
        default_pacing_time_window: TimeDelta::from_native_microsecond_bits(5_000),
    };
    let events = [
        NetworkControllerReplayEvent::NetworkAvailability(NetworkAvailability {
            at_time: Timestamp::from_native_microsecond_bits(200_000),
            network_available: true,
        }),
        NetworkControllerReplayEvent::TransportPacketsFeedback(TransportPacketsFeedback {
            feedback_time: Timestamp::from_native_microsecond_bits(300_000),
            packet_feedbacks: vec![PacketResult {
                receive_time: Timestamp::from_native_microsecond_bits(150_000),
                ..Default::default()
            }],
            ..Default::default()
        }),
        NetworkControllerReplayEvent::ProcessInterval(ProcessInterval {
            at_time: Timestamp::from_native_microsecond_bits(350_000),
            ..Default::default()
        }),
    ];
    assert_eq!(
        network_controller_replay_for_test(&environment, &factory, config.clone(), &events)
            .unwrap()
            .len(),
        3
    );
    assert_eq!(manual_clock.now(), Duration::from_micros(350_000));
    assert_eq!(
        *times.lock().unwrap(),
        vec![100_000, 200_000, 300_000, 350_000]
    );

    let backwards = [NetworkControllerReplayEvent::NetworkAvailability(
        NetworkAvailability {
            at_time: Timestamp::from_native_microsecond_bits(300_000),
            network_available: true,
        },
    )];
    assert_eq!(
        network_controller_replay_for_test(&environment, &factory, config.clone(), &backwards),
        Err(NetworkControllerReplayError::InvalidNativeInput)
    );
    assert_eq!(manual_clock.now(), Duration::from_micros(350_000));
    assert_eq!(
        *times.lock().unwrap(),
        vec![100_000, 200_000, 300_000, 350_000]
    );

    let config_at_now = NetworkControllerReplayConfig {
        constraints: TargetRateConstraints {
            at_time: Timestamp::from_native_microsecond_bits(350_000),
            ..config.constraints
        },
        ..config
    };
    assert_eq!(
        network_controller_replay_for_test(&environment, &factory, config_at_now, &backwards),
        Err(NetworkControllerReplayError::InvalidNativeInput)
    );
    assert_eq!(manual_clock.now(), Duration::from_micros(350_000));
    assert_eq!(
        *times.lock().unwrap(),
        vec![100_000, 200_000, 300_000, 350_000, 350_000]
    );
}
