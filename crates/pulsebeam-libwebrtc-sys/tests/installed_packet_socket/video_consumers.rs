//! Installed products consumed by an actual VP8 sender on the general sockets.
use super::*;

const SELECTED_RATE: u32 = 117_000;
type Origin = (FieldTrialsSnapshot, ClockTimeBounds, i64);

#[derive(Default)]
struct FecObserved {
    created: AtomicUsize,
    dropped: AtomicUsize,
    origins: Mutex<Vec<Origin>>,
    callbacks: Mutex<Vec<FecProtectionCallback>>,
    methods: Mutex<Vec<(bool, bool)>>,
    encoding: Mutex<Vec<FecEncodingData>>,
    rates: Mutex<Vec<FecRateUpdate>>,
    encoded: Mutex<Vec<(usize, FecEncodedFrameType)>>,
    mask_queries: AtomicUsize,
    protection: Mutex<Vec<Result<FecProtectionResponse, FecProtectionCallbackError>>>,
}
struct FecFactory(Arc<FecObserved>);
struct FecProduct {
    observed: Arc<FecObserved>,
    callback: Mutex<Option<FecProtectionCallback>>,
    _environment: FecControllerEnvironment,
}
impl FecControllerFactoryProvider for FecFactory {
    fn create_fec_controller(
        &mut self,
        environment: FecControllerEnvironment,
    ) -> FecControllerCreation {
        self.0.origins.lock().unwrap().push((
            environment.field_trials(),
            environment.environment().clock_time_bounds(),
            environment.time_us(),
        ));
        self.0.created.fetch_add(1, Ordering::SeqCst);
        FecControllerCreation::Provider(Box::new(FecProduct {
            observed: self.0.clone(),
            callback: Mutex::new(None),
            _environment: environment,
        }))
    }
}
impl FecController for FecProduct {
    fn set_protection_callback(&self, callback: Option<FecProtectionCallback>) {
        if let Some(callback) = &callback {
            self.observed
                .callbacks
                .lock()
                .unwrap()
                .push(callback.clone());
        }
        *self.callback.lock().unwrap() = callback;
    }
    fn set_protection_method(&self, fec: bool, nack: bool) {
        self.observed.methods.lock().unwrap().push((fec, nack));
    }
    fn set_encoding_data(&self, data: FecEncodingData) {
        self.observed.encoding.lock().unwrap().push(data);
    }
    fn update_fec_rates(&self, update: FecRateUpdate) -> u32 {
        let bitrate = update.estimated_bitrate_bps;
        self.observed.rates.lock().unwrap().push(update);
        let callback = self.callback.lock().unwrap().clone();
        if let Some(callback) = callback {
            // This is a live SDK controller invocation on its worker. Release
            // all provider locks first. Do not mutate retransmission policy
            // synchronously here: UpdateFecRates holds the sender mutex.
            let params = FecProtectionParams {
                fec_rate: 0,
                max_fec_frames: 1,
                fec_mask_type: FecMaskType::Random,
            };
            let result = callback.request(params, params);
            self.observed.protection.lock().unwrap().push(result);
        }
        bitrate
    }
    fn update_with_encoded_data(&self, length: usize, kind: FecEncodedFrameType) {
        self.observed.encoded.lock().unwrap().push((length, kind));
    }
    fn use_loss_vector_mask(&self) -> bool {
        self.observed.mask_queries.fetch_add(1, Ordering::SeqCst);
        false
    }
}
impl Drop for FecProduct {
    fn drop(&mut self) {
        self.observed.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct AllocatorObserved {
    created: AtomicUsize,
    dropped: AtomicUsize,
    origins: Mutex<Vec<Origin>>,
    codecs: Mutex<Vec<VideoCodecConfiguration>>,
    allocations: Mutex<Vec<(VideoBitrateAllocationParameters, VideoBitrateAllocation)>>,
}
struct AllocatorFactory(Arc<AllocatorObserved>);
struct AllocatorProduct {
    observed: Arc<AllocatorObserved>,
    _environment: VideoBitrateAllocatorEnvironment,
}
impl VideoBitrateAllocatorFactoryProvider for AllocatorFactory {
    fn create(
        &mut self,
        environment: VideoBitrateAllocatorEnvironment,
        codec: VideoCodecConfiguration,
    ) -> VideoBitrateAllocatorProduct {
        self.0.origins.lock().unwrap().push((
            environment.field_trials(),
            environment.environment().clock_time_bounds(),
            environment.time_us(),
        ));
        self.0.codecs.lock().unwrap().push(codec);
        self.0.created.fetch_add(1, Ordering::SeqCst);
        VideoBitrateAllocatorProduct::Provider(Box::new(AllocatorProduct {
            observed: self.0.clone(),
            _environment: environment,
        }))
    }
}
impl VideoBitrateAllocator for AllocatorProduct {
    fn allocate(&mut self, parameters: VideoBitrateAllocationParameters) -> VideoBitrateAllocation {
        let mut allocation = VideoBitrateAllocation::default();
        allocation.layer_bitrates_bps[0][0] =
            Some(if parameters.total_bitrate == DataRate::BitsPerSecond(0) {
                0
            } else {
                SELECTED_RATE
            });
        self.observed
            .allocations
            .lock()
            .unwrap()
            .push((parameters, allocation));
        allocation
    }
}
impl Drop for AllocatorProduct {
    fn drop(&mut self) {
        self.observed.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

fn assert_origins(origins: &[Origin], harness: &Harness) {
    assert!(
        !origins.is_empty(),
        "no actual SDK product creation environment"
    );
    for (trials, bounds, time_us) in origins {
        assert_eq!(trials.lookup("Provider-Transport"), "Selected");
        assert_eq!(
            trials.lookup("WebRTC-RFC8888CongestionControlFeedback"),
            "Disabled"
        );
        assert_eq!(*bounds, harness.environment.clock_time_bounds());
        assert!(
            *time_us >= 1_000_000
                && *time_us <= harness.environment.timestamp().microseconds().unwrap()
        );
    }
}

#[test]
fn actual_video_sender_consumes_selected_fec_allocator_and_real_transport_feedback() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let cert_a = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let cert_b = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let harness = Harness::new();
    let input = EncodedVideoInput::new_for_format(VideoCodecFormat::new("VP8")).unwrap();
    let fec_observed = Arc::new(FecObserved::default());
    let fec = FecControllerFactory::from_provider(FecFactory(fec_observed.clone()));
    let allocator_observed = Arc::new(AllocatorObserved::default());
    let allocator =
        VideoBitrateAllocatorFactory::from_provider(AllocatorFactory(allocator_observed.clone()));
    let estimator_observed = Arc::new(EstimatorObserved::default());
    let estimator =
        NetworkStateEstimatorFactory::from_provider(EstimatorFactory(estimator_observed.clone()));
    let controller = NetworkControllerFactory::goog_cc(
        GoogCcFactoryConfig::new(false).network_state_estimator_factory(&estimator),
    );
    let alice_ip = Ipv4Addr::new(10, 84, 0, 1);
    let bob_ip = Ipv4Addr::new(10, 84, 0, 2);
    let (factory_a, sockets_a, managers_a) =
        harness.factory_with_selected_media(alice_ip, Some(&controller), Some(&input), Some(&fec));
    let (factory_b, sockets_b, managers_b) =
        harness.factory_with_controller(bob_ip, Some(&controller));
    let config = |certificate: Certificate| {
        let mut config = RtcConfiguration::default();
        config.options.certificates.push(certificate);
        config.options.tcp_candidate_policy = TcpCandidatePolicy::Disabled;
        config
    };
    let mut alice = factory_a
        .create_peer_connection_with_dependencies(
            config(cert_a),
            PeerConnectionDependencies::default().video_bitrate_allocator_factory(&allocator),
        )
        .unwrap();
    let mut bob = factory_b
        .create_peer_connection_with_configuration(config(cert_b))
        .unwrap();
    let mut source = input.create_source(&factory_a).unwrap();
    let track = source.create_track(&factory_a, "installed-vp8").unwrap();
    let transceiver = alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let mut alice_events = Vec::new();
    let mut bob_events = Vec::new();
    let offer = harness
        .completed(&alice, alice.create_offer().unwrap(), &mut alice_events)
        .unwrap();
    assert!(offer.sdp.contains("transport-cc") && !offer.sdp.contains("ack ccfb"));
    harness.completed(
        &alice,
        alice.set_local_description(offer).unwrap(),
        &mut alice_events,
    );
    let offer = harness.gathered(&alice, &mut alice_events, alice_ip);
    harness.completed(
        &bob,
        bob.set_remote_description(offer).unwrap(),
        &mut bob_events,
    );
    let answer = harness
        .completed(&bob, bob.create_answer().unwrap(), &mut bob_events)
        .unwrap();
    assert!(answer.sdp.contains("transport-cc") && !answer.sdp.contains("ack ccfb"));
    harness.completed(
        &bob,
        bob.set_local_description(answer).unwrap(),
        &mut bob_events,
    );
    let answer = harness.gathered(&bob, &mut bob_events, bob_ip);
    harness.completed(
        &alice,
        alice.set_remote_description(answer).unwrap(),
        &mut alice_events,
    );
    let mut connected_a = false;
    let mut connected_b = false;
    let mut sinks = Vec::new();
    for _ in 0..10_000 {
        alice_events.extend(std::iter::from_fn(|| alice.try_next_event()));
        bob_events.extend(std::iter::from_fn(|| bob.try_next_event()));
        for event in alice_events.drain(..) {
            connected_a |= matches!(
                event,
                PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected)
            );
        }
        for event in bob_events.drain(..) {
            match event {
                PeerConnectionEvent::Track(transceiver) => sinks.push(
                    transceiver
                        .receiver()
                        .track()
                        .expect("real video track")
                        .attach_sink()
                        .unwrap(),
                ),
                PeerConnectionEvent::ConnectionStateChanged(ConnectionState::Connected) => {
                    connected_b = true
                }
                _ => {}
            }
        }
        if connected_a && connected_b && !sinks.is_empty() {
            break;
        }
        harness.step();
    }
    assert!(connected_a && connected_b && !sinks.is_empty());
    let mut decoded = 0;
    let mut applied_rate = None;
    for index in 0..240 {
        source
            .push_encoded(EncodedVideoAccessUnit {
                data: include_bytes!("../fixtures/simulation-red.vp8").to_vec(),
                width: 32,
                height: 32,
                timestamp_us: harness.world.now().as_micros().try_into().unwrap(),
                key_frame: true,
                qp: None,
                metadata: EncodedVideoMetadata {
                    codec: EncodedVideoCodec::Vp8 {
                        non_reference: false,
                        layer_sync: false,
                        key_index: None,
                    },
                    temporal_index: None,
                    simulcast_index: None,
                    spatial_index: None,
                    end_of_picture: true,
                },
            })
            .unwrap();
        for _ in 0..34 {
            harness.step();
            for sink in &sinks {
                while let Some(received) = sink.try_next_received_frame() {
                    assert_eq!((received.frame.width, received.frame.height), (32, 32));
                    decoded += 1;
                }
            }
            if let Some(rate) = source.latest_rate_control() {
                if rate.target_bitrate_bps == SELECTED_RATE
                    && rate.target_layer_bitrates_bps[0][0] == Some(SELECTED_RATE)
                {
                    applied_rate = Some(rate);
                }
            }
        }
        if index >= 60
            && decoded > 0
            && applied_rate.is_some()
            && estimator_observed
                .feedback
                .lock()
                .unwrap()
                .iter()
                .any(|feedback| !feedback.packet_feedbacks.is_empty())
            && fec_observed
                .protection
                .lock()
                .unwrap()
                .iter()
                .any(|result| {
                    result
                        .as_ref()
                        .is_ok_and(|response| response.sent_video_rate_bps > 0)
                })
        {
            break;
        }
    }
    assert!(
        decoded > 0,
        "actual VP8 decoder must receive video, not just a Track event"
    );
    assert!(source.take_encoder_error().is_none());
    let applied =
        applied_rate.expect("selected allocator output must reach native encoder SetRates");
    assert!(applied.bitrate_bps > 0 && applied.layer_bitrates_bps[0][0].unwrap_or(0) > 0);
    assert!(
        allocator_observed
            .allocations
            .lock()
            .unwrap()
            .iter()
            .any(|(_, allocation)| allocation.sum_bps() == Some(SELECTED_RATE))
    );
    assert!(
        allocator_observed
            .codecs
            .lock()
            .unwrap()
            .iter()
            .any(|codec| codec.codec_type == VideoCodecType::Vp8
                && codec.width == 32
                && codec.height == 32)
    );
    assert!(fec_observed.created.load(Ordering::SeqCst) > 0);
    assert!(!fec_observed.methods.lock().unwrap().is_empty());
    assert!(
        fec_observed
            .encoding
            .lock()
            .unwrap()
            .iter()
            .any(|data| data.width == 32 && data.height == 32 && data.max_payload_size > 0)
    );
    assert!(
        fec_observed
            .encoded
            .lock()
            .unwrap()
            .iter()
            .any(|(length, kind)| *length > 0 && *kind == FecEncodedFrameType::Key)
    );
    assert!(fec_observed.mask_queries.load(Ordering::SeqCst) > 0);
    assert!(!fec_observed.rates.lock().unwrap().is_empty());
    let protection = fec_observed.protection.lock().unwrap();
    assert!(!protection.is_empty());
    assert!(
        protection.iter().all(|result| result
            .as_ref()
            .is_ok_and(|response| response.native_result == 0)),
        "all live protection requests must reach the SDK sender: {protection:?}"
    );
    assert!(protection.iter().any(|result| {
        result
            .as_ref()
            .is_ok_and(|response| response.sent_video_rate_bps > 0)
    }));
    drop(protection);
    assert!(
        estimator_observed
            .feedback
            .lock()
            .unwrap()
            .iter()
            .any(|feedback| !feedback.packet_feedbacks.is_empty())
    );
    assert_origins(&fec_observed.origins.lock().unwrap(), &harness);
    assert_origins(&allocator_observed.origins.lock().unwrap(), &harness);
    let stats_operation = alice.request_stats().unwrap();
    let mut stats = None;
    for _ in 0..2_000 {
        while let Some(event) = alice.try_next_event() {
            if let PeerConnectionEvent::Stats(snapshot) = event {
                if snapshot.operation_id == stats_operation {
                    stats = Some(snapshot);
                }
            }
        }
        if stats.is_some() {
            break;
        }
        harness.step();
    }
    let stats = stats.expect("real outbound video stats");
    let primary_ssrcs: Vec<u32> = stats
        .records
        .iter()
        .filter_map(|record| match record {
            PeerStatsRecord::OutboundRtp(record)
                if record.kind.as_deref() == Some(b"video".as_slice())
                    && record.packets_sent.unwrap_or(0) > 0 =>
            {
                record.ssrc.and_then(|ssrc| u32::try_from(ssrc).ok())
            }
            _ => None,
        })
        .collect();
    assert!(!primary_ssrcs.is_empty());
    assert!(
        harness
            .link
            .rtp_headers
            .lock()
            .unwrap()
            .iter()
            .any(|(_, ssrc, _)| primary_ssrcs.contains(ssrc)),
        "native sender stats must match actual primary RTP on the selected socket"
    );
    alice.close().unwrap();
    bob.close().unwrap();
    source.close().unwrap();
    drop((
        sinks,
        source,
        track,
        transceiver,
        alice,
        bob,
        factory_a,
        factory_b,
        input,
    ));
    assert_eq!(
        fec_observed.created.load(Ordering::SeqCst),
        fec_observed.dropped.load(Ordering::SeqCst)
    );
    assert_eq!(
        allocator_observed.created.load(Ordering::SeqCst),
        allocator_observed.dropped.load(Ordering::SeqCst)
    );
    assert_eq!(
        estimator_observed.created.load(Ordering::SeqCst),
        estimator_observed.dropped.load(Ordering::SeqCst)
    );
    let params = FecProtectionParams::default();
    let callbacks = fec_observed.callbacks.lock().unwrap();
    assert!(!callbacks.is_empty());
    assert!(callbacks.iter().all(
        |callback| callback.request(params, params) == Err(FecProtectionCallbackError::Retired)
    ));
    drop(callbacks);
    // Retained actual creation-trial views remain callable after sender/factory
    // teardown; they do not retain either sender or its protection capability.
    assert_origins(&fec_observed.origins.lock().unwrap(), &harness);
    assert_origins(&allocator_observed.origins.lock().unwrap(), &harness);
    assert_eq!(
        harness.link.created.load(Ordering::SeqCst),
        harness.link.dropped.load(Ordering::SeqCst)
    );
    assert!(
        !fec.has_failed()
            && !allocator.failed()
            && !controller.has_failed()
            && !estimator.has_failed()
    );
    assert!(
        !sockets_a.has_failed()
            && !sockets_b.has_failed()
            && !managers_a.has_failed()
            && !managers_b.has_failed()
    );
}
