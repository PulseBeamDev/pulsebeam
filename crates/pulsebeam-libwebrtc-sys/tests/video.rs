use std::{
    net::Ipv4Addr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[path = "support/non_trickle.rs"]
mod non_trickle;
#[path = "support/video_sink_wants.rs"]
mod video_sink_wants;

use pulsebeam_webrtc_sys::{
    CodecError, CodecSupport, ConnectionState, DecodedImageCallback, EncodedH264Input,
    EncodedImageCallback, EncodedVideoAccessUnit, EncodedVideoCodec, EncodedVideoFrame,
    EncodedVideoInput, EncodedVideoMetadata, Environment, H264AccessUnit, ManualClock, Nv12Planes,
    OperationId, PeerConfiguration, PeerConnection, PeerConnectionEvent, PeerConnectionFactory,
    PeerErrorKind, PeerStatsRecord, RtcErrorType, RtpSenderOperationError, RtpTransceiver,
    RtpTransceiverDirection, SessionDescription, SimulatedNetwork, VideoCodecFormat, VideoDecoder,
    VideoDecoderFactory, VideoDecoderFactoryHandle, VideoDecoderInfo, VideoDecoderSettings,
    VideoEncoder, VideoEncoderFactory, VideoEncoderFactoryHandle, VideoEncoderInfo,
    VideoEncoderSettings, VideoFrame, VideoFrameBuffer, VideoFrameType, VideoPlane,
    VideoRateControl, VideoResolution, VideoRotation, VideoSimulcastStream, VideoSourceState,
    VideoTrackState,
};

#[derive(Default)]
struct Counters {
    encoder_factory: AtomicUsize,
    encoder_create: AtomicUsize,
    encode: AtomicUsize,
    encoder_release: AtomicUsize,
    encoder_rotation: AtomicU32,
    encoder_settings: Mutex<Vec<VideoEncoderSettings>>,
    encoder_rates: Mutex<Vec<VideoRateControl>>,
    encoder_fps_allocation: Option<[Vec<u8>; 5]>,
    decoder_factory: AtomicUsize,
    decoder_create: AtomicUsize,
    decoder_settings: Mutex<Vec<VideoDecoderSettings>>,
    decode: AtomicUsize,
    decoder_release: AtomicUsize,
    decoder_rtp_timestamp: AtomicU32,
}

fn h264() -> VideoCodecFormat {
    VideoCodecFormat::new("H264")
        .with_parameter("level-asymmetry-allowed", "1")
        .with_parameter("packetization-mode", "1")
        .with_parameter("profile-level-id", "42e01f")
}

struct EncoderFactory {
    counters: Arc<Counters>,
    fail: bool,
}

impl VideoEncoderFactory for EncoderFactory {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        self.counters.encoder_factory.fetch_add(1, Ordering::SeqCst);
        vec![h264()]
    }

    fn query_support(
        &self,
        format: &VideoCodecFormat,
        _: Option<&str>,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        CodecSupport {
            supported: format.name.eq_ignore_ascii_case("H264"),
            power_efficient: false,
        }
    }

    fn create(&self, format: &VideoCodecFormat) -> Result<Box<dyn VideoEncoder>, CodecError> {
        self.counters.encoder_create.fetch_add(1, Ordering::SeqCst);
        if self.fail || !format.name.eq_ignore_ascii_case("H264") {
            Err(CodecError::ConstructionFailed)
        } else {
            Ok(Box::new(TestEncoder(self.counters.clone())))
        }
    }
}

struct TestEncoder(Arc<Counters>);

impl VideoEncoder for TestEncoder {
    fn initialize(&mut self, settings: VideoEncoderSettings) -> Result<(), CodecError> {
        self.0.encoder_settings.lock().unwrap().push(settings);
        Ok(())
    }

    fn encode(
        &mut self,
        frame: VideoFrame,
        _: &[VideoFrameType],
        callback: EncodedImageCallback,
    ) -> Result<(), CodecError> {
        self.0.encode.fetch_add(1, Ordering::SeqCst);
        self.0
            .encoder_rotation
            .store(frame.rotation as u32, Ordering::SeqCst);
        callback.emit(&EncodedVideoFrame {
            // A minimal Annex-B SPS/PPS/IDR sample. The injected decoder owns
            // interpretation; the native H.264 RTP path needs valid framing.
            data: vec![
                0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x1e, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0,
                0xf0, 0x3c, 0x48, 0x99, 0x20, 0, 0, 0, 1, 0x68, 0xcb, 0x80, 0xc4, 0xb2, 0, 0, 1,
                0x65, 0x88, 0x84, 0xf1, 0x18, 0xa0, 0, 0x20, 0x5b, 0x1c, 0, 4, 7, 0xe3, 0x80, 0,
                0x80, 0xfe,
            ],
            width: frame.width,
            height: frame.height,
            rtp_timestamp: frame.rtp_timestamp,
            frame_type: VideoFrameType::Key,
            qp: Some(20),
        })
    }

    fn set_rates(&mut self, rates: VideoRateControl) -> Result<(), CodecError> {
        self.0.encoder_rates.lock().unwrap().push(rates);
        Ok(())
    }

    fn release(&mut self) -> Result<(), CodecError> {
        self.0.encoder_release.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn info(&self) -> VideoEncoderInfo {
        VideoEncoderInfo {
            implementation_name: "test-only H264-shaped encoder".into(),
            hardware_accelerated: false,
            supports_native_handle: false,
            supports_simulcast: false,
            fps_allocation: Some(
                self.0
                    .encoder_fps_allocation
                    .clone()
                    .unwrap_or_else(|| [vec![255], vec![], vec![], vec![], vec![]]),
            ),
        }
    }
}

struct DecoderFactory(Arc<Counters>, VideoCodecFormat);

impl VideoDecoderFactory for DecoderFactory {
    fn supported_formats(&self) -> Vec<VideoCodecFormat> {
        self.0.decoder_factory.fetch_add(1, Ordering::SeqCst);
        vec![self.1.clone()]
    }

    fn query_support(
        &self,
        format: &VideoCodecFormat,
        _: bool,
        _: Option<VideoResolution>,
    ) -> CodecSupport {
        CodecSupport {
            supported: format == &self.1,
            power_efficient: false,
        }
    }

    fn create(&self, format: &VideoCodecFormat) -> Result<Box<dyn VideoDecoder>, CodecError> {
        self.0.decoder_create.fetch_add(1, Ordering::SeqCst);
        if format == &self.1 {
            Ok(Box::new(TestDecoder(self.0.clone())))
        } else {
            Err(CodecError::UnsupportedFormat)
        }
    }
}

struct TestDecoder(Arc<Counters>);

impl VideoDecoder for TestDecoder {
    fn configure(&mut self, settings: VideoDecoderSettings) -> Result<(), CodecError> {
        self.0.decoder_settings.lock().unwrap().push(settings);
        Ok(())
    }

    fn decode(
        &mut self,
        frame: EncodedVideoFrame,
        callback: DecodedImageCallback,
    ) -> Result<(), CodecError> {
        self.0.decode.fetch_add(1, Ordering::SeqCst);
        self.0
            .decoder_rtp_timestamp
            .store(frame.rtp_timestamp, Ordering::SeqCst);
        callback.emit(&VideoFrame::i420(
            frame.width,
            frame.height,
            synthetic_i420(frame.width, frame.height),
            i64::from(frame.rtp_timestamp) * 1000,
            frame.rtp_timestamp,
        )?)
    }

    fn release(&mut self) -> Result<(), CodecError> {
        self.0.decoder_release.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn info(&self) -> VideoDecoderInfo {
        VideoDecoderInfo {
            implementation_name: "test-only H264-shaped decoder".into(),
            hardware_accelerated: false,
        }
    }
}

struct Pair {
    clock: ManualClock,
    network: SimulatedNetwork,
    alice_factory: PeerConnectionFactory,
    bob_factory: PeerConnectionFactory,
    alice: PeerConnection,
    bob: PeerConnection,
    _alice_endpoint: pulsebeam_webrtc_sys::NetworkEndpoint,
    _bob_endpoint: pulsebeam_webrtc_sys::NetworkEndpoint,
}

impl Pair {
    fn new(counters: Arc<Counters>, fail_encoder: bool) -> Self {
        Self::new_with_encoder(counters, fail_encoder, None)
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn new_builtin() -> Self {
        let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
        let environment = Environment::builder().clock(&clock).build().unwrap();
        let network = SimulatedNetwork::new(&clock).unwrap();
        let alice_endpoint = network
            .register_endpoint(Ipv4Addr::new(10, 22, 0, 1).into())
            .unwrap();
        let bob_endpoint = network
            .register_endpoint(Ipv4Addr::new(10, 22, 0, 2).into())
            .unwrap();
        let encoder = VideoEncoderFactoryHandle::builtin().unwrap();
        let decoder = VideoDecoderFactoryHandle::builtin().unwrap();
        assert!(
            encoder
                .supported_formats()
                .iter()
                .any(|format| format.name == "H264")
        );
        assert!(
            decoder
                .supported_formats()
                .iter()
                .any(|format| format.name == "H264")
        );
        assert!(encoder.query_support(&h264(), None, None).supported);
        assert!(decoder.query_support(&h264(), false, None).supported);
        let alice_factory = PeerConnectionFactory::builder()
            .environment(environment.clone())
            .network_manager(alice_endpoint.network_manager().unwrap())
            .packet_socket_factory(alice_endpoint.packet_socket_factory().unwrap())
            .video_encoder_factory(encoder.clone())
            .video_decoder_factory(decoder.clone())
            .build()
            .unwrap();
        let bob_factory = PeerConnectionFactory::builder()
            .environment(environment)
            .network_manager(bob_endpoint.network_manager().unwrap())
            .packet_socket_factory(bob_endpoint.packet_socket_factory().unwrap())
            .video_encoder_factory(encoder)
            .video_decoder_factory(decoder)
            .build()
            .unwrap();
        // The peer factories, not caller-owned selection variables, retain the
        // actual codec implementations through use and teardown.
        let alice = alice_factory
            .create_peer_connection(PeerConfiguration::default())
            .unwrap();
        let bob = bob_factory
            .create_peer_connection(PeerConfiguration::default())
            .unwrap();
        Self {
            clock,
            network,
            alice_factory,
            bob_factory,
            alice,
            bob,
            _alice_endpoint: alice_endpoint,
            _bob_endpoint: bob_endpoint,
        }
    }

    fn new_with_encoder(
        counters: Arc<Counters>,
        fail_encoder: bool,
        encoder: Option<VideoEncoderFactoryHandle>,
    ) -> Self {
        Self::new_with_codec(counters, fail_encoder, encoder, h264())
    }
    fn new_with_codec(
        counters: Arc<Counters>,
        fail_encoder: bool,
        encoder: Option<VideoEncoderFactoryHandle>,
        decoder_format: VideoCodecFormat,
    ) -> Self {
        Self::new_with_dependencies(
            counters,
            fail_encoder,
            encoder,
            decoder_format,
            Default::default(),
        )
    }

    fn new_with_dependencies(
        counters: Arc<Counters>,
        fail_encoder: bool,
        encoder: Option<VideoEncoderFactoryHandle>,
        decoder_format: VideoCodecFormat,
        dependencies: pulsebeam_webrtc_sys::PeerConnectionDependencies,
    ) -> Self {
        Self::new_with_fec(
            counters,
            fail_encoder,
            encoder,
            decoder_format,
            dependencies,
            None,
        )
    }

    fn new_with_fec(
        counters: Arc<Counters>,
        fail_encoder: bool,
        encoder: Option<VideoEncoderFactoryHandle>,
        decoder_format: VideoCodecFormat,
        dependencies: pulsebeam_webrtc_sys::PeerConnectionDependencies,
        fec: Option<&pulsebeam_webrtc_sys::FecControllerFactory>,
    ) -> Self {
        Self::new_with_call_factories(
            counters,
            fail_encoder,
            encoder,
            decoder_format,
            dependencies,
            fec,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_call_factories(
        counters: Arc<Counters>,
        fail_encoder: bool,
        encoder: Option<VideoEncoderFactoryHandle>,
        decoder_format: VideoCodecFormat,
        dependencies: pulsebeam_webrtc_sys::PeerConnectionDependencies,
        fec: Option<&pulsebeam_webrtc_sys::FecControllerFactory>,
        network_controller: Option<&pulsebeam_webrtc_sys::NetworkControllerFactory>,
    ) -> Self {
        let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
        let mut environment = Environment::builder().clock(&clock);
        if network_controller.is_some() {
            environment = environment.field_trials(
                pulsebeam_webrtc_sys::FieldTrials::parse(
                    "WebRTC-Bwe-InjectedCongestionController/Enabled/",
                )
                .unwrap(),
            );
        }
        let environment = environment.build().unwrap();
        let network = SimulatedNetwork::new(&clock).unwrap();
        let alice_endpoint = network
            .register_endpoint(Ipv4Addr::new(10, 22, 0, 1).into())
            .unwrap();
        let bob_endpoint = network
            .register_endpoint(Ipv4Addr::new(10, 22, 0, 2).into())
            .unwrap();
        let alice_factory = factory_with_call_factories(
            environment.clone(),
            &alice_endpoint,
            counters.clone(),
            fail_encoder,
            encoder,
            h264(),
            fec,
            network_controller,
        );
        let bob_factory = factory(
            environment,
            &bob_endpoint,
            counters,
            false,
            None,
            decoder_format,
        );
        let alice = alice_factory
            .create_peer_connection_with_dependencies(
                PeerConfiguration::default().into(),
                dependencies,
            )
            .unwrap();
        let bob = bob_factory
            .create_peer_connection(PeerConfiguration::default())
            .unwrap();
        Self {
            clock,
            network,
            alice_factory,
            bob_factory,
            alice,
            bob,
            _alice_endpoint: alice_endpoint,
            _bob_endpoint: bob_endpoint,
        }
    }

    fn progress(&self) -> (Vec<PeerConnectionEvent>, Vec<PeerConnectionEvent>) {
        let alice_events = drain(&self.alice);
        let bob_events = drain(&self.bob);
        while let Some(packet) = self.network.next_packet() {
            self.network.deliver(packet.id).unwrap();
        }
        self.clock.advance(Duration::from_millis(1)).unwrap();
        thread::yield_now();
        (alice_events, bob_events)
    }

    fn negotiate(&self) -> Vec<PeerConnectionEvent> {
        self.negotiate_with_receive_rids(false)
    }

    fn negotiate_with_receive_rids(&self, receive_rids: bool) -> Vec<PeerConnectionEvent> {
        self.negotiate_with_extensions_and_rids(receive_rids, false)
    }

    fn negotiate_with_extensions_and_rids(
        &self,
        receive_rids: bool,
        layered: bool,
    ) -> Vec<PeerConnectionEvent> {
        if layered {
            for transceiver in self.alice.video_transceivers().unwrap() {
                enable_layered_extensions(&transceiver);
            }
        }
        let mut alice_events = Vec::new();
        let mut bob_events = Vec::new();
        let offer = completion_description(
            &self.alice,
            self.alice.create_offer().unwrap(),
            &mut alice_events,
        );
        completion(
            &self.alice,
            self.alice.set_local_description(offer.clone()).unwrap(),
            &mut alice_events,
        );
        let gathered = non_trickle::gathered_local_description(
            &self.alice,
            &self.clock,
            &self.network,
            &mut alice_events,
        );
        assert_eq!(gathered.kind, offer.kind);
        completion(
            &self.bob,
            self.bob.set_remote_description(gathered).unwrap(),
            &mut bob_events,
        );
        if layered {
            for transceiver in self.bob.video_transceivers().unwrap() {
                enable_layered_extensions(&transceiver);
            }
        }
        let answer = completion_description(
            &self.bob,
            self.bob.create_answer().unwrap(),
            &mut bob_events,
        );
        completion(
            &self.bob,
            self.bob.set_local_description(answer.clone()).unwrap(),
            &mut bob_events,
        );
        let mut gathered = non_trickle::gathered_local_description(
            &self.bob,
            &self.clock,
            &self.network,
            &mut bob_events,
        );
        assert_eq!(gathered.kind, answer.kind);
        if receive_rids {
            // Simulate an SFU accepting both offered RIDs in its answer.
            gathered
                .sdp
                .push_str("a=rid:f recv\r\na=rid:h recv\r\na=simulcast:recv f;h\r\n");
        }
        completion(
            &self.alice,
            self.alice.set_remote_description(gathered).unwrap(),
            &mut alice_events,
        );
        bob_events
    }
}

const DD_URI: &str =
    "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";
const VLA_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00";

fn enable_layered_extensions(transceiver: &pulsebeam_webrtc_sys::RtpTransceiver) {
    use pulsebeam_webrtc_sys::RtpHeaderExtensionDirection;
    let mut extensions = transceiver.header_extensions_to_negotiate().unwrap();
    for uri in [DD_URI, VLA_URI] {
        let extension = extensions
            .iter_mut()
            .find(|extension| extension.uri() == uri)
            .expect("native layered extension");
        extension.direction = RtpHeaderExtensionDirection::SendReceive;
    }
    transceiver
        .set_header_extensions_to_negotiate(&extensions)
        .unwrap();
}

fn factory(
    environment: Environment,
    endpoint: &pulsebeam_webrtc_sys::NetworkEndpoint,
    counters: Arc<Counters>,
    fail_encoder: bool,
    encoder: Option<VideoEncoderFactoryHandle>,
    decoder_format: VideoCodecFormat,
) -> PeerConnectionFactory {
    factory_with_fec(
        environment,
        endpoint,
        counters,
        fail_encoder,
        encoder,
        decoder_format,
        None,
    )
}

fn factory_with_fec(
    environment: Environment,
    endpoint: &pulsebeam_webrtc_sys::NetworkEndpoint,
    counters: Arc<Counters>,
    fail_encoder: bool,
    encoder: Option<VideoEncoderFactoryHandle>,
    decoder_format: VideoCodecFormat,
    fec: Option<&pulsebeam_webrtc_sys::FecControllerFactory>,
) -> PeerConnectionFactory {
    factory_with_call_factories(
        environment,
        endpoint,
        counters,
        fail_encoder,
        encoder,
        decoder_format,
        fec,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn factory_with_call_factories(
    environment: Environment,
    endpoint: &pulsebeam_webrtc_sys::NetworkEndpoint,
    counters: Arc<Counters>,
    fail_encoder: bool,
    encoder: Option<VideoEncoderFactoryHandle>,
    decoder_format: VideoCodecFormat,
    fec: Option<&pulsebeam_webrtc_sys::FecControllerFactory>,
    network_controller: Option<&pulsebeam_webrtc_sys::NetworkControllerFactory>,
) -> PeerConnectionFactory {
    let mut builder = PeerConnectionFactory::builder()
        .environment(environment)
        .network_manager(endpoint.network_manager().unwrap())
        .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
        .video_encoder_factory(encoder.unwrap_or_else(|| {
            VideoEncoderFactoryHandle::new(EncoderFactory {
                counters: counters.clone(),
                fail: fail_encoder,
            })
            .unwrap()
        }))
        .video_decoder_factory(
            VideoDecoderFactoryHandle::new(DecoderFactory(counters, decoder_format)).unwrap(),
        );
    if let Some(factory) = fec {
        builder = builder.fec_controller_factory(factory);
    }
    if let Some(factory) = network_controller {
        builder = builder.network_controller_factory(factory);
    }
    builder.build().unwrap()
}
#[test]
fn call_execution_resources_reach_actual_encoder_and_decoder_without_codec_substitution() {
    use pulsebeam_webrtc_sys::{CodecCoreBudget, ExecutionResources};
    // These caller codecs are probes for the real SDK Call/send/receive path,
    // not evidence of built-in codec worker control or deterministic execution.
    let mut detected_cores = None;
    for selected in [None, Some(1), Some(i32::MAX), Some(2), None] {
        let resources = ExecutionResources {
            codec_core_budget: selected.map(|cores| CodecCoreBudget::new(cores).unwrap()),
        };
        let counters = Arc::new(Counters::default());
        let clock = ManualClock::new(Duration::from_secs(1)).unwrap();
        let environment = Environment::builder().clock(&clock).build().unwrap();
        let network = SimulatedNetwork::new(&clock).unwrap();
        let alice_endpoint = network
            .register_endpoint(Ipv4Addr::new(10, 22, 0, 1).into())
            .unwrap();
        let bob_endpoint = network
            .register_endpoint(Ipv4Addr::new(10, 22, 0, 2).into())
            .unwrap();
        let factory = |endpoint: &pulsebeam_webrtc_sys::NetworkEndpoint| {
            PeerConnectionFactory::builder()
                .environment(environment.clone())
                .execution_resources(resources)
                .network_manager(endpoint.network_manager().unwrap())
                .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
                .video_encoder_factory(
                    VideoEncoderFactoryHandle::new(EncoderFactory {
                        counters: counters.clone(),
                        fail: false,
                    })
                    .unwrap(),
                )
                .video_decoder_factory(
                    VideoDecoderFactoryHandle::new(DecoderFactory(counters.clone(), h264()))
                        .unwrap(),
                )
                .build()
                .unwrap()
        };
        let alice_factory = factory(&alice_endpoint);
        let bob_factory = factory(&bob_endpoint);
        let alice = alice_factory
            .create_peer_connection(PeerConfiguration::default())
            .unwrap();
        let bob = bob_factory
            .create_peer_connection(PeerConfiguration::default())
            .unwrap();
        let pair = Pair {
            clock,
            network,
            alice_factory,
            bob_factory,
            alice,
            bob,
            _alice_endpoint: alice_endpoint,
            _bob_endpoint: bob_endpoint,
        };
        let source = pair.alice_factory.create_video_source().unwrap();
        let track = pair
            .alice_factory
            .create_video_track("resource-probe", &source)
            .unwrap();
        pair.alice
            .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
            .unwrap();
        pair.negotiate();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut index = 0u32;
        while counters.decode.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "resource probe never reached native decode"
            );
            source
                .push_frame(
                    &VideoFrame::i420(
                        16,
                        16,
                        synthetic_i420(16, 16),
                        2_000_000 + i64::from(index) * 33_333,
                        index * 3000,
                    )
                    .unwrap(),
                )
                .unwrap();
            index += 1;
            pair.progress();
            pair.clock.advance(Duration::from_millis(32)).unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        let encoded = counters.encoder_settings.lock().unwrap();
        let decoded = counters.decoder_settings.lock().unwrap();
        assert!(!encoded.is_empty() && !decoded.is_empty());
        if let Some(cores) = selected {
            assert!(
                encoded
                    .iter()
                    .all(|settings| settings.cores == cores as u32)
            );
            assert!(
                decoded
                    .iter()
                    .all(|settings| settings.cores == cores as u32)
            );
        } else {
            assert!(encoded.iter().all(|settings| settings.cores > 0));
            let expected = *detected_cores.get_or_insert(encoded[0].cores);
            assert!(encoded.iter().all(|settings| settings.cores == expected));
            assert!(decoded.iter().all(|settings| settings.cores == expected));
        }
    }
}

fn synthetic_i420(width: u32, height: u32) -> Vec<u8> {
    let len = (width * height + 2 * width.div_ceil(2) * height.div_ceil(2)) as usize;
    (0..len).map(|index| index as u8).collect()
}

#[test]
fn strided_nv12_converts_odd_dimensions_and_rejects_bad_planes() {
    let y = VideoPlane {
        stride: 5,
        data: vec![1, 2, 3, 99, 99, 4, 5, 6, 99, 99, 7, 8, 9, 99, 99],
    };
    let uv = VideoPlane {
        stride: 6,
        data: vec![10, 20, 30, 40, 99, 99, 50, 60, 70, 80, 99, 99],
    };
    let frame = VideoFrame::nv12(
        3,
        3,
        Nv12Planes {
            y: y.clone(),
            uv: uv.clone(),
        },
        123,
        456,
    )
    .unwrap();
    assert_eq!(
        frame.buffer.as_bytes(),
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 30, 50, 70, 20, 40, 60, 80]
    );
    assert_eq!(
        VideoFrameBuffer::i420_strided(
            3,
            3,
            y.clone(),
            VideoPlane {
                stride: 3,
                data: vec![10, 30, 99, 50, 70, 99]
            },
            VideoPlane {
                stride: 3,
                data: vec![20, 40, 99, 60, 80, 99]
            },
        )
        .unwrap(),
        frame.buffer,
    );
    let packed = frame.to_nv12().unwrap();
    assert_eq!(packed.y.stride, 3);
    assert_eq!(packed.uv.stride, 4);
    assert_eq!(packed.uv.data, vec![10, 20, 30, 40, 50, 60, 70, 80]);
    assert_eq!(VideoFrame::nv12(3, 3, packed, 123, 456).unwrap(), frame);
    assert_eq!(
        VideoFrame::nv12(
            3,
            3,
            Nv12Planes {
                y: y.clone(),
                uv: VideoPlane {
                    stride: 3,
                    data: uv.data.clone()
                }
            },
            0,
            0
        ),
        Err(CodecError::InvalidFrame),
    );
    assert_eq!(
        VideoFrame::nv12(
            3,
            3,
            Nv12Planes {
                y: VideoPlane {
                    stride: 5,
                    data: y.data[..14].to_vec()
                },
                uv
            },
            0,
            0
        ),
        Err(CodecError::InvalidFrame),
    );
    assert_eq!(
        VideoFrame::nv12(
            0,
            3,
            Nv12Planes {
                y: y.clone(),
                uv: VideoPlane {
                    stride: 4,
                    data: vec![0; 8]
                }
            },
            0,
            0
        ),
        Err(CodecError::InvalidFrame),
    );
    assert_eq!(
        VideoFrame::nv12(
            u32::MAX,
            u32::MAX,
            Nv12Planes {
                y,
                uv: VideoPlane {
                    stride: 4,
                    data: vec![0; 8]
                }
            },
            0,
            0
        ),
        Err(CodecError::InvalidFrame),
    );
}

fn drain(peer: &PeerConnection) -> Vec<PeerConnectionEvent> {
    std::iter::from_fn(|| peer.try_next_event()).collect()
}

fn completion(
    peer: &PeerConnection,
    id: OperationId,
    side_events: &mut Vec<PeerConnectionEvent>,
) -> Option<SessionDescription> {
    for _ in 0..1_000_000 {
        while let Some(event) = peer.try_next_event() {
            match event {
                PeerConnectionEvent::OperationComplete(done) if done.operation_id == id => {
                    return done.result.unwrap();
                }
                event => side_events.push(event),
            }
        }
        thread::yield_now();
    }
    panic!("operation {} did not complete", id.get());
}

fn completion_description(
    peer: &PeerConnection,
    id: OperationId,
    side_events: &mut Vec<PeerConnectionEvent>,
) -> SessionDescription {
    completion(peer, id, side_events).unwrap()
}

fn take_remote_transceiver(events: &mut Vec<PeerConnectionEvent>) -> Option<RtpTransceiver> {
    events
        .iter()
        .position(|event| matches!(event, PeerConnectionEvent::Track(_)))
        .map(|index| match events.swap_remove(index) {
            PeerConnectionEvent::Track(transceiver) => transceiver,
            _ => unreachable!(),
        })
}

#[test]
fn video_sink_bounds_retention_and_reports_loss() {
    let counters = Arc::new(Counters::default());
    let mut pair = Pair::new(counters, false);
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("bounded", &source)
        .unwrap();
    let mut sink = track.attach_sink().unwrap();
    let mut invalid = VideoFrame::i420(2, 2, vec![0; 6], 0, 0).unwrap();
    for (width, height) in [(65536, 65536), (i32::MAX as u32, 1), (1, u32::MAX)] {
        invalid.width = width;
        invalid.height = height;
        assert_eq!(source.push_frame(&invalid), Err(CodecError::InvalidFrame));
        assert_eq!(source.state(), VideoSourceState::Live);
        assert_eq!(sink.try_next_frame(), None);
        assert_eq!(sink.dropped_frames(), 0);
    }
    for index in 0..6 {
        source
            .push_frame(&VideoFrame::i420(2, 2, vec![index as u8; 6], index, index as u32).unwrap())
            .unwrap();
    }
    assert_eq!(sink.dropped_frames(), 2);
    for index in 2..6 {
        assert_eq!(
            sink.try_next_frame(),
            Some(VideoFrame::i420(2, 2, vec![index as u8; 6], index, index as u32).unwrap())
        );
    }
    assert_eq!(sink.try_next_frame(), None);
    sink.close().unwrap();
    assert_eq!(sink.dropped_frames(), 2);
    pair.alice.close().unwrap();
    pair.bob.close().unwrap();
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn builtin_h264_encodes_and_decodes_over_real_peer_transport() {
    let mut pair = Pair::new_builtin();
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("builtin-h264", &source)
        .unwrap();
    let transceiver = pair
        .alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let codecs: Vec<_> = pair
        .alice
        .video_sender_capabilities()
        .unwrap()
        .into_iter()
        .filter(|codec| codec.format().name == "H264")
        .collect();
    assert!(!codecs.is_empty());
    transceiver.set_codec_preferences(&codecs).unwrap();
    let mut events = pair.negotiate();
    let remote = (0..2_000_000)
        .find_map(|_| {
            if let Some(remote) = take_remote_transceiver(&mut events) {
                return Some(remote);
            }
            events.extend(pair.progress().1);
            None
        })
        .expect("native H.264 remote transceiver");
    let remote_track = remote.receiver().track().unwrap();
    let mut sink = remote_track.attach_sink().unwrap();
    let mut pixels = vec![48; 64 * 64];
    pixels.extend(vec![96; 32 * 32]);
    pixels.extend(vec![160; 32 * 32]);
    let mut received = None;
    // Repeated input permits ordinary native asynchronous codec/transport
    // startup. Only an actual decoded frame with representative contents passes.
    for index in 0..20 {
        let timestamp_us = i64::try_from(pair.clock.now().as_micros()).unwrap();
        source
            .push_frame(
                &VideoFrame::i420(64, 64, pixels.clone(), timestamp_us, 90_000 + index * 3_000)
                    .unwrap(),
            )
            .unwrap();
        received = (0..100_000).find_map(|_| {
            pair.progress();
            sink.try_next_frame()
        });
        if received.is_some() {
            break;
        }
    }
    let received = received.expect("bundled OpenH264/FFmpeg decoded output");
    assert_eq!((received.width, received.height), (64, 64));
    let output = received.buffer.as_bytes();
    assert_eq!(output.len(), pixels.len());
    assert!(
        output
            .iter()
            .zip(&pixels)
            .all(|(&actual, &expected)| actual.abs_diff(expected) <= 2)
    );
    sink.close().unwrap();
    pair.alice.close().unwrap();
    pair.bob.close().unwrap();
}

const DIRECT_H264_KEY: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x1e, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0xf0, 0x3c,
    0x48, 0x99, 0x20, 0, 0, 0, 1, 0x68, 0xcb, 0x80, 0xc4, 0xb2, 0, 0, 0, 1, 0x65, 0x88, 0x84, 0xf1,
    0x18, 0xa0, 0, 0x20, 0x5b, 0x1c, 0, 4, 7, 0xe3, 0x80, 0, 0x80, 0xfe,
];

#[test]
fn direct_encoded_h264_input_reaches_remote_without_decode() {
    let counters = Arc::new(Counters::default());
    let input = EncodedH264Input::new().unwrap();
    let mut pair = Pair::new_with_encoder(counters.clone(), false, Some(input.encoder_factory()));
    assert_eq!(
        input.create_source(&pair.bob_factory).err().unwrap().kind,
        PeerErrorKind::InvalidParameter
    );
    let mut source = input.create_source(&pair.alice_factory).unwrap();
    assert_eq!(
        source
            .create_track(&pair.bob_factory, "wrong")
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::InvalidParameter
    );
    let track = source
        .create_track(&pair.alice_factory, "encoded-h264")
        .unwrap();
    assert_eq!(
        pair.alice
            .add_video_transceiver_with_rids(
                &track,
                RtpTransceiverDirection::SendOnly,
                &["low".into(), "high".into()]
            )
            .err()
            .unwrap()
            .kind,
        PeerErrorKind::UnsupportedParameter
    );
    let transceiver = pair
        .alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let mut unsupported = transceiver.sender().parameters().unwrap();
    unsupported.encodings[0].scale_resolution_down_by = Some(2.0);
    assert_eq!(
        transceiver
            .sender()
            .set_parameters(unsupported)
            .unwrap_err()
            .kind,
        PeerErrorKind::UnsupportedParameter
    );
    let codecs: Vec<_> = pair
        .alice
        .video_sender_capabilities()
        .unwrap()
        .into_iter()
        .filter(|codec| codec.format().name == "H264")
        .collect();
    assert!(!codecs.is_empty());
    transceiver.set_codec_preferences(&codecs).unwrap();
    let mut bob_events = pair.negotiate();
    let remote = (0..2_000_000)
        .find_map(|_| {
            if let Some(remote) = take_remote_transceiver(&mut bob_events) {
                return Some(remote);
            }
            bob_events.extend(pair.progress().1);
            None
        })
        .expect("encoded-video remote transceiver did not arrive");
    let receiver = remote.receiver();
    let mut sink = receiver.attach_encoded_sink().unwrap();
    for _ in 0..100_000 {
        pair.progress();
        if transceiver.current_direction() == Some(RtpTransceiverDirection::SendOnly) {
            break;
        }
    }
    // Access unit bytes, not a generated encoding of a placeholder raw frame.
    let frame = H264AccessUnit {
        data: DIRECT_H264_KEY.to_vec(),
        width: 16,
        height: 16,
        timestamp_us: 2_000_000,
        key_frame: true,
        qp: Some(20),
    };
    source.push(frame.clone()).unwrap();
    let received = (0..2_000_000)
        .find_map(|_| {
            pair.progress();
            sink.try_next_frame()
        })
        .unwrap_or_else(|| {
            panic!(
                "direct encoded input missing: pending={}, dropped={}, direction={:?}",
                source.pending_frames(),
                source.dropped_frames(),
                transceiver.current_direction()
            )
        });
    assert_eq!(received.data, frame.data);
    assert!(received.key_frame);
    assert!(received.mime_type.eq_ignore_ascii_case("video/H264"));
    assert_ne!(received.ssrc, 0);
    assert_eq!(counters.encode.load(Ordering::SeqCst), 0);
    assert_eq!(counters.decode.load(Ordering::SeqCst), 0);
    assert_eq!(source.dropped_frames(), 0);
    assert!(source.latest_rate_control().is_some());
    transceiver.sender().request_keyframe(&[]).unwrap();
    for _ in 0..1000 {
        pair.progress();
    }
    source
        .push(H264AccessUnit {
            data: vec![0, 0, 0, 1, 0x41, 0x88, 0x84, 0xf1],
            key_frame: false,
            timestamp_us: 3_000_000,
            ..frame.clone()
        })
        .unwrap();
    let requested = (0..100_000).any(|_| {
        pair.progress();
        source.take_keyframe_request()
    });
    assert!(
        requested,
        "sender keyframe feedback did not reach encoded source"
    );
    source
        .push(H264AccessUnit {
            timestamp_us: 4_000_000,
            ..frame.clone()
        })
        .unwrap();
    let next = (0..2_000_000)
        .find_map(|_| {
            pair.progress();
            sink.try_next_frame()
        })
        .expect("keyframe did not resume encoded delivery");
    assert_eq!(next.data, frame.data);
    // Exercise the full owning native operation separately from the older
    // active-video convenience method, and observe the actual encoder callback.
    let _ = source.take_keyframe_request();
    transceiver.sender().generate_keyframe(&[]).unwrap();
    for _ in 0..1000 {
        pair.progress();
    }
    source
        .push(H264AccessUnit {
            data: vec![0, 0, 0, 1, 0x41, 0x88, 0x84, 0xf1],
            key_frame: false,
            timestamp_us: 5_000_000,
            ..frame.clone()
        })
        .unwrap();
    assert!(
        (0..100_000).any(|_| {
            pair.progress();
            source.take_keyframe_request()
        }),
        "native GenerateKeyFrame did not reach the actual encoded source callback"
    );
    source
        .push(H264AccessUnit {
            timestamp_us: 6_000_000,
            ..frame.clone()
        })
        .unwrap();
    let native_recovery = (0..2_000_000)
        .find_map(|_| {
            pair.progress();
            sink.try_next_frame()
        })
        .expect("native keyframe request did not resume encoded delivery");
    assert_eq!(native_recovery.data, frame.data);
    assert!(native_recovery.key_frame);
    assert_eq!(
        source.push(H264AccessUnit {
            key_frame: false,
            ..frame.clone()
        }),
        Err(CodecError::InvalidFrame)
    );
    source.close().unwrap();
    assert_eq!(source.push(frame), Err(CodecError::Released));
    sink.close();
    pair.alice.close().unwrap();
    pair.bob.close().unwrap();
}

#[test]
fn direct_h264_l1t3_preserves_input_and_rejects_native_fallback() {
    for (select_mode, layered) in [(false, true), (true, false), (true, true)] {
        let counters = Arc::new(Counters::default());
        let input = EncodedH264Input::new_l1t3([64, 128, 255]).unwrap();
        let mut pair =
            Pair::new_with_encoder(counters.clone(), false, Some(input.encoder_factory()));
        let mut source = input.create_source(&pair.alice_factory).unwrap();
        let track = source
            .create_track(&pair.alice_factory, "temporal-h264")
            .unwrap();
        let transceiver = pair
            .alice
            .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
            .unwrap();
        let codecs: Vec<_> = pair
            .alice
            .video_sender_capabilities()
            .unwrap()
            .into_iter()
            .filter(|codec| codec.format().name == "H264")
            .collect();
        transceiver.set_codec_preferences(&codecs).unwrap();
        let defaults = transceiver.header_extensions_to_negotiate().unwrap();
        for uri in [DD_URI, VLA_URI] {
            assert_eq!(
                defaults
                    .iter()
                    .find(|extension| extension.uri() == uri)
                    .unwrap()
                    .direction,
                pulsebeam_webrtc_sys::RtpHeaderExtensionDirection::Stopped
            );
        }
        assert!(transceiver.set_header_extensions_to_negotiate(&[]).is_err());
        let mut invalid = defaults.clone();
        invalid
            .iter_mut()
            .find(|extension| extension.uri() == "urn:ietf:params:rtp-hdrext:sdes:mid")
            .unwrap()
            .direction = pulsebeam_webrtc_sys::RtpHeaderExtensionDirection::Stopped;
        assert!(
            transceiver
                .set_header_extensions_to_negotiate(&invalid)
                .is_err()
        );
        assert_eq!(
            transceiver.header_extensions_to_negotiate().unwrap(),
            defaults
        );
        let mut events = pair.negotiate_with_extensions_and_rids(false, layered);
        for uri in [DD_URI, VLA_URI] {
            let extensions = transceiver.negotiated_header_extensions().unwrap();
            let extension = extensions
                .iter()
                .find(|extension| extension.uri() == uri)
                .unwrap();
            assert_eq!(
                extension.direction,
                if layered {
                    pulsebeam_webrtc_sys::RtpHeaderExtensionDirection::SendReceive
                } else {
                    pulsebeam_webrtc_sys::RtpHeaderExtensionDirection::Stopped
                }
            );
            if layered {
                assert!(extension.preferred_id().is_some());
            }
        }
        let remote = (0..2_000_000)
            .find_map(|_| {
                if let Some(remote) = take_remote_transceiver(&mut events) {
                    return Some(remote);
                }
                events.extend(pair.progress().1);
                None
            })
            .unwrap();
        let mut sink = remote.receiver().attach_encoded_sink().unwrap();
        if select_mode {
            let sender = transceiver.sender();
            let mut parameters = sender.parameters().unwrap();
            parameters.encodings[0].scalability_mode = Some("L1T3".into());
            let result = sender.set_parameters(parameters);
            if !layered {
                assert_eq!(
                    result.unwrap_err().kind,
                    PeerErrorKind::UnsupportedParameter
                );
                assert_eq!(source.pending_frames(), 0);
                assert_eq!(source.dropped_frames(), 0);
                source.close().unwrap();
                sink.close();
                pair.alice.close().unwrap();
                assert_eq!(
                    transceiver
                        .header_extensions_to_negotiate()
                        .unwrap_err()
                        .kind,
                    PeerErrorKind::Closed
                );
                pair.bob.close().unwrap();
                continue;
            }
            result.unwrap();
        }
        // Give threaded role queues time to run without racing simulated
        // receiver timeout/PLI timers by milliseconds on every spin.
        let progress = || {
            drain(&pair.alice);
            drain(&pair.bob);
            while let Some(packet) = pair.network.next_packet() {
                pair.network.deliver(packet.id).unwrap();
            }
            pair.clock.advance(Duration::from_micros(1)).unwrap();
            thread::yield_now();
        };
        let mut unit = EncodedVideoAccessUnit {
            data: DIRECT_H264_KEY.to_vec(),
            width: 16,
            height: 16,
            timestamp_us: 2_000_000,
            key_frame: true,
            qp: Some(20),
            metadata: EncodedVideoMetadata {
                codec: EncodedVideoCodec::H264 {
                    base_layer_sync: false,
                },
                simulcast_index: None,
                spatial_index: None,
                temporal_index: Some(0),
                end_of_picture: true,
            },
        };
        let mut invalid = unit.clone();
        invalid.metadata.temporal_index = Some(3);
        assert_eq!(source.push_encoded(invalid), Err(CodecError::InvalidFrame));
        assert_eq!(source.pending_frames(), 0);
        source.push_encoded(unit.clone()).unwrap();
        if !select_mode {
            let error = (0..2_000_000)
                .find_map(|_| {
                    progress();
                    source.take_encoder_error()
                })
                .unwrap();
            assert_eq!(error, CodecError::InvalidConfiguration);
            assert_eq!(source.dropped_frames(), 1);
            assert!(sink.try_next_frame().is_none());
        } else {
            let received = (0..2_000_000)
                .find_map(|_| {
                    progress();
                    sink.try_next_frame()
                })
                .unwrap_or_else(|| {
                    panic!(
                        "L1T3 key missing: error={:?}, pending={}",
                        source.take_encoder_error(),
                        source.pending_frames()
                    )
                });
            assert_eq!(received.data, unit.data);
            assert_eq!(source.take_encoder_error(), None, "key callback completion");
            let rates = source.latest_rate_control().expect("native rate guidance");
            assert_eq!(
                rates
                    .layer_bitrates_bps
                    .iter()
                    .flatten()
                    .flatten()
                    .copied()
                    .map(u64::from)
                    .sum::<u64>(),
                u64::from(rates.bitrate_bps)
            );
            assert!(rates.layer_bitrates_bps[0][0].is_some());
            assert!(rates.layer_bitrates_bps[0][1].is_some());
            assert!(rates.layer_bitrates_bps[0][2].is_some());
            assert!(rates.layer_bitrates_bps[0][3].is_none());
            assert!(
                rates.layer_bitrates_bps[1..]
                    .iter()
                    .flatten()
                    .all(Option::is_none)
            );
            let mut base_id = received.frame_id.expect("negotiated native DD frame id");
            let mut last_id = base_id;
            assert_eq!(received.spatial_index, Some(0));
            assert_eq!(received.temporal_index, Some(0));
            assert!(received.dependencies.is_empty());
            assert_eq!(
                received.decode_target_indications,
                vec![pulsebeam_webrtc_sys::DecodeTargetIndication::Switch; 4]
            );
            let ssrc = received.ssrc;
            let payload_type = received.payload_type;
            let mut rejected = 0;
            source.take_keyframe_request();
            for (number, temporal) in [2, 1, 2, 0].into_iter().enumerate() {
                unit.data = vec![0, 0, 0, 1, 0x41, 0x88, 0x84, 0xf1, number as u8];
                unit.timestamp_us += 33_333;
                unit.key_frame = false;
                unit.metadata.temporal_index = Some(temporal);
                unit.metadata.codec = EncodedVideoCodec::H264 {
                    base_layer_sync: temporal != 0,
                };
                let mut delivered = None;
                for _ in 0..4 {
                    source.push_encoded(unit.clone()).unwrap();
                    let outcome = (0..2_000_000)
                        .find_map(|_| {
                            progress();
                            if let Some(error) = source.take_encoder_error() {
                                return Some(Err(error));
                            }
                            sink.try_next_frame().map(Ok)
                        })
                        .expect("delta or observable encoder rejection");
                    match outcome {
                        Ok(frame) => {
                            delivered = Some(frame);
                            break;
                        }
                        Err(error) => {
                            assert_eq!(error, CodecError::InvalidFrame);
                            assert!(source.take_keyframe_request());
                            rejected += 1;
                            // The test producer, not the binding, supplies a
                            // compressed keyframe in response to native feedback.
                            let mut key = unit.clone();
                            key.timestamp_us += 33_333;
                            key.data = DIRECT_H264_KEY.to_vec();
                            key.key_frame = true;
                            key.metadata.temporal_index = Some(0);
                            key.metadata.codec = EncodedVideoCodec::H264 {
                                base_layer_sync: false,
                            };
                            source.push_encoded(key.clone()).unwrap();
                            let frame = (0..2_000_000)
                                .find_map(|_| {
                                    progress();
                                    sink.try_next_frame()
                                })
                                .expect("requested keyframe delivered");
                            assert_eq!(frame.data, key.data);
                            assert!(frame.key_frame);
                            assert!(frame.dependencies.is_empty());
                            base_id = frame.frame_id.expect("requested key native DD id");
                            assert!(base_id > last_id);
                            last_id = base_id;
                            assert_eq!(source.take_encoder_error(), None);
                            source.take_keyframe_request();
                            unit.timestamp_us = key.timestamp_us + 33_333;
                        }
                    }
                }
                let received = delivered.expect("delta after honoring native keyframe feedback");
                assert_eq!(received.data, unit.data);
                assert!(!received.key_frame);
                assert_eq!(received.ssrc, ssrc);
                assert_eq!(received.payload_type, payload_type);
                assert_eq!(received.spatial_index, Some(0));
                assert_eq!(received.temporal_index, Some(i32::from(temporal)));
                let id = received.frame_id.expect("delta native DD frame id");
                assert!(id > last_id);
                last_id = id;
                assert_eq!(received.dependencies, vec![base_id]);
                assert_eq!(received.decode_target_indications.len(), 4);
                for (target, indication) in received.decode_target_indications.iter().enumerate() {
                    assert_eq!(
                        *indication,
                        if target < usize::from(temporal) {
                            pulsebeam_webrtc_sys::DecodeTargetIndication::NotPresent
                        } else {
                            pulsebeam_webrtc_sys::DecodeTargetIndication::Switch
                        }
                    );
                }
            }
            assert_eq!(source.dropped_frames(), rejected);
            assert_eq!(source.take_encoder_error(), None);
        }
        assert_eq!(counters.encode.load(Ordering::SeqCst), 0);
        assert_eq!(counters.decode.load(Ordering::SeqCst), 0);
        source.close().unwrap();
        sink.close();
        pair.alice.close().unwrap();
        pair.bob.close().unwrap();
    }
}

fn verify_direct_encoded_codec(
    name: &str,
    unit: &[u8],
    width: u32,
    height: u32,
    codec: EncodedVideoCodec,
) {
    let counters = Arc::new(Counters::default());
    let format = match name {
        "VP9" => VideoCodecFormat::new(name).with_parameter("profile-id", "0"),
        "AV1" => VideoCodecFormat::new(name)
            .with_parameter("level-idx", "5")
            .with_parameter("profile", "0")
            .with_parameter("tier", "0"),
        "H265" => VideoCodecFormat::new(name)
            .with_parameter("level-id", "93")
            .with_parameter("tx-mode", "SRST"),
        _ => VideoCodecFormat::new(name),
    };
    let input = EncodedVideoInput::new_for_format(VideoCodecFormat::new(name)).unwrap();
    let mut pair = Pair::new_with_codec(
        counters.clone(),
        false,
        Some(input.encoder_factory()),
        format.clone(),
    );
    let mut source = input.create_source(&pair.alice_factory).unwrap();
    let track = source
        .create_track(&pair.alice_factory, "encoded-input")
        .unwrap();
    let transceiver = pair
        .alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let codecs: Vec<_> = pair
        .alice
        .video_sender_capabilities()
        .unwrap()
        .into_iter()
        .filter(|codec| codec.format() == &format)
        .collect();
    assert!(!codecs.is_empty());
    transceiver.set_codec_preferences(&codecs).unwrap();
    let mut bob_events = pair.negotiate();
    let remote = (0..2_000_000)
        .find_map(|_| {
            if let Some(remote) = take_remote_transceiver(&mut bob_events) {
                return Some(remote);
            }
            bob_events.extend(pair.progress().1);
            None
        })
        .expect("remote encoded transceiver did not arrive");
    let mut sink = remote.receiver().attach_encoded_sink().unwrap();
    for _ in 0..100_000 {
        pair.progress();
        if transceiver.current_direction() == Some(RtpTransceiverDirection::SendOnly) {
            break;
        }
    }
    let metadata = EncodedVideoMetadata {
        codec,
        simulcast_index: None,
        spatial_index: None,
        temporal_index: None,
        end_of_picture: true,
    };
    assert_eq!(
        source.push_encoded(EncodedVideoAccessUnit {
            data: unit.to_vec(),
            width,
            height,
            timestamp_us: 2_000_000,
            key_frame: true,
            qp: None,
            metadata: EncodedVideoMetadata {
                codec: EncodedVideoCodec::H264 {
                    base_layer_sync: false,
                },
                ..metadata
            },
        }),
        Err(CodecError::InvalidFrame)
    );
    source
        .push_encoded(EncodedVideoAccessUnit {
            data: unit.to_vec(),
            width,
            height,
            timestamp_us: 2_000_000,
            key_frame: true,
            qp: None,
            metadata,
        })
        .unwrap();
    let received = (0..2_000_000)
        .find_map(|_| {
            pair.progress();
            sink.try_next_frame()
        })
        .unwrap_or_else(|| panic!("encoded {name} frame did not reach receiver: pending={}, dropped={}, sink_dropped={}, direction={:?}, rate={:?}", source.pending_frames(), source.dropped_frames(), sink.dropped_frames(), transceiver.current_direction(), source.latest_rate_control()));
    assert_eq!(received.data, unit);
    assert!(received.key_frame);
    assert!(
        received
            .mime_type
            .eq_ignore_ascii_case(&format!("video/{name}"))
    );
    assert_eq!(counters.encode.load(Ordering::SeqCst), 0);
    assert_eq!(counters.decode.load(Ordering::SeqCst), 0);
    source.close().unwrap();
    sink.close();
    pair.alice.close().unwrap();
    pair.bob.close().unwrap();
}

#[test]
fn direct_encoded_vp8_is_packetized_without_a_software_encoder() {
    // Valid 16x16 VP8 keyframe generated with libvpx; IVF framing removed.
    let unit = [
        16, 2, 0, 157, 1, 42, 16, 0, 16, 0, 0, 71, 8, 133, 133, 136, 153, 132, 136, 2, 2, 0, 12,
        13, 96, 0, 254, 255, 171, 80, 128,
    ];
    verify_direct_encoded_codec(
        "VP8",
        &unit,
        16,
        16,
        EncodedVideoCodec::Vp8 {
            non_reference: false,
            layer_sync: false,
            key_index: None,
        },
    );
}

#[test]
fn direct_encoded_vp9_and_av1_are_packetized() {
    // Single keyframes generated by libvpx-vp9 and SVT-AV1; IVF framing removed.
    let vp9 = [
        130, 73, 131, 66, 0, 0, 240, 0, 246, 6, 56, 36, 28, 24, 74, 0, 0, 32, 64, 0, 34, 155, 255,
        255, 149, 118, 246, 223, 244, 172, 146, 21, 235, 239, 55, 79, 202, 128, 145, 200, 72, 205,
        184, 252, 166, 144, 210, 80, 128, 68, 193, 71, 141, 184, 4, 0, 0,
    ];
    verify_direct_encoded_codec(
        "VP9",
        &vp9,
        16,
        16,
        EncodedVideoCodec::Vp9 {
            first_frame_in_picture: true,
            inter_picture_predicted: false,
            flexible_mode: false,
            num_spatial_layers: 1,
            inter_layer_predicted: false,
            temporal_up_switch: false,
        },
    );
    let av1 = [
        10, 11, 2, 0, 0, 5, 21, 127, 252, 74, 249, 0, 64, 50, 14, 16, 0, 243, 130, 63, 254, 105,
        131, 0, 0, 8, 148, 19, 216,
    ];
    verify_direct_encoded_codec("AV1", &av1, 64, 64, EncodedVideoCodec::Av1);
}

#[test]
fn direct_encoded_h265_is_packetized() {
    // Valid 64x64 HEVC IDR access unit from x265 with encoder SEI disabled.
    let unit = [
        0, 0, 0, 1, 64, 1, 12, 1, 255, 255, 1, 96, 0, 0, 3, 0, 144, 0, 0, 3, 0, 0, 3, 0, 30, 149,
        152, 9, 0, 0, 0, 1, 66, 1, 1, 1, 96, 0, 0, 3, 0, 144, 0, 0, 3, 0, 0, 3, 0, 30, 160, 32,
        129, 5, 150, 86, 105, 36, 202, 240, 22, 128, 128, 0, 0, 3, 0, 128, 0, 0, 3, 0, 132, 0, 0,
        0, 1, 68, 1, 193, 114, 180, 34, 64, 0, 0, 0, 1, 40, 1, 175, 19, 128, 230, 104, 227, 255,
        253, 23, 207, 199, 246, 207,
    ];
    verify_direct_encoded_codec("H265", &unit, 64, 64, EncodedVideoCodec::H265);
}

#[test]
fn direct_encoded_sources_keep_stream_identity_and_isolate_teardown() {
    let input = EncodedH264Input::new().unwrap();
    let counters = Arc::new(Counters::default());
    let mut pair = Pair::new_with_encoder(counters.clone(), false, Some(input.encoder_factory()));
    let mut first = input.create_source(&pair.alice_factory).unwrap();
    let mut second = input.create_source(&pair.alice_factory).unwrap();
    assert_ne!(first.stream_id(), second.stream_id());
    let first_track = first
        .create_track(&pair.alice_factory, "first-encoded")
        .unwrap();
    let second_track = second
        .create_track(&pair.alice_factory, "second-encoded")
        .unwrap();
    let first_transceiver = pair
        .alice
        .add_video_transceiver(&first_track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let second_transceiver = pair
        .alice
        .add_video_transceiver(&second_track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let codecs: Vec<_> = pair
        .alice
        .video_sender_capabilities()
        .unwrap()
        .into_iter()
        .filter(|codec| codec.format().name == "H264")
        .collect();
    first_transceiver.set_codec_preferences(&codecs).unwrap();
    second_transceiver.set_codec_preferences(&codecs).unwrap();
    let mut events = pair.negotiate();
    let mut remote = Vec::new();
    for _ in 0..2_000_000 {
        while let Some(transceiver) = take_remote_transceiver(&mut events) {
            remote.push((
                transceiver.mid().unwrap(),
                transceiver.receiver().attach_encoded_sink().unwrap(),
            ));
        }
        if remote.len() == 2 {
            break;
        }
        events.extend(pair.progress().1);
    }
    assert_eq!(remote.len(), 2);
    let first_mid = first_transceiver.mid().unwrap();
    let second_mid = second_transceiver.mid().unwrap();
    let first_index = remote
        .iter()
        .position(|(mid, _)| *mid == first_mid)
        .unwrap();
    let second_index = remote
        .iter()
        .position(|(mid, _)| *mid == second_mid)
        .unwrap();
    assert_ne!(first_index, second_index);
    // Negotiated directions do not prove ICE/DTLS transport readiness. An
    // initial frame submitted before the connection is ready may be dropped.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        pair.progress();
        if pair.alice.peer_connection_state() == ConnectionState::Connected
            && pair.bob.peer_connection_state() == ConnectionState::Connected
            && first_transceiver.current_direction() == Some(RtpTransceiverDirection::SendOnly)
            && second_transceiver.current_direction() == Some(RtpTransceiverDirection::SendOnly)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "two-stream transport did not become ready: alice={:?}, bob={:?}",
            pair.alice.peer_connection_state(),
            pair.bob.peer_connection_state(),
        );
    }
    let first_data = vec![
        0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x0a, 0xd9, 0x1e, 0x84, 0, 0, 3, 0, 4, 0, 0, 3, 0, 0xf0,
        0x3c, 0x48, 0x99, 0x20, 0, 0, 0, 1, 0x68, 0xcb, 0x80, 0xc4, 0xb2, 0, 0, 0, 1, 0x65, 0x88,
        0x84, 0xf1, 0x18, 0xa0, 0, 0x20, 0x5b, 0x1c, 0, 4, 7, 0xe3, 0x80, 0, 0x80, 0xfe,
    ];
    let mut second_data = first_data.clone();
    *second_data.last_mut().unwrap() = 2;
    let frame = |data: Vec<u8>, timestamp_us| H264AccessUnit {
        data,
        width: 16,
        height: 16,
        timestamp_us,
        key_frame: true,
        qp: None,
    };
    first.push(frame(first_data.clone(), 2_000_000)).unwrap();
    second.push(frame(second_data.clone(), 2_000_000)).unwrap();
    let mut received = [None, None];
    for _ in 0..2_000_000 {
        pair.progress();
        for (index, (_, sink)) in remote.iter_mut().enumerate() {
            if received[index].is_none() {
                received[index] = sink.try_next_frame();
            }
        }
        if received.iter().all(Option::is_some) {
            break;
        }
    }
    assert_eq!(
        received[first_index]
            .as_ref()
            .expect("first stream missing")
            .data,
        first_data
    );
    assert_eq!(
        received[second_index]
            .as_ref()
            .expect("second stream missing")
            .data,
        second_data
    );
    first.close().unwrap();
    second.push(frame(second_data.clone(), 3_000_000)).unwrap();
    let still_live = (0..2_000_000)
        .find_map(|_| {
            pair.progress();
            remote[second_index].1.try_next_frame()
        })
        .expect("closing first source interrupted second");
    assert_eq!(still_live.data, second_data);
    assert_eq!(counters.encode.load(Ordering::SeqCst), 0);
    assert_eq!(counters.decode.load(Ordering::SeqCst), 0);
    for (_, sink) in &mut remote {
        sink.close();
    }
    second.close().unwrap();
    pair.alice.close().unwrap();
    pair.bob.close().unwrap();
}

#[derive(Default)]
struct AllocationObservations {
    codecs: Mutex<Vec<pulsebeam_webrtc_sys::VideoCodecConfiguration>>,
    environment: Mutex<Option<pulsebeam_webrtc_sys::VideoBitrateAllocatorEnvironment>>,
    parameters: Mutex<Vec<pulsebeam_webrtc_sys::VideoBitrateAllocationParameters>>,
    legacy: Mutex<Vec<bool>>,
    factory_drops: AtomicUsize,
    product_drops: AtomicUsize,
}

struct AllocationFactory(Arc<AllocationObservations>, std::cell::Cell<usize>);
impl pulsebeam_webrtc_sys::VideoBitrateAllocatorFactoryProvider for AllocationFactory {
    fn create(
        &mut self,
        environment: pulsebeam_webrtc_sys::VideoBitrateAllocatorEnvironment,
        codec: pulsebeam_webrtc_sys::VideoCodecConfiguration,
    ) -> pulsebeam_webrtc_sys::VideoBitrateAllocatorProduct {
        self.1.set(self.1.get() + 1);
        *self.0.environment.lock().unwrap() = Some(environment);
        self.0.codecs.lock().unwrap().push(codec);
        pulsebeam_webrtc_sys::VideoBitrateAllocatorProduct::Provider(Box::new(AllocationProduct(
            self.0.clone(),
            std::cell::Cell::new(0),
        )))
    }
}
impl Drop for AllocationFactory {
    fn drop(&mut self) {
        self.0.factory_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct AllocationProduct(Arc<AllocationObservations>, std::cell::Cell<usize>);
impl pulsebeam_webrtc_sys::VideoBitrateAllocator for AllocationProduct {
    fn allocate(
        &mut self,
        parameters: pulsebeam_webrtc_sys::VideoBitrateAllocationParameters,
    ) -> pulsebeam_webrtc_sys::VideoBitrateAllocation {
        self.1.set(self.1.get() + 1);
        self.0.parameters.lock().unwrap().push(parameters);
        let mut result = pulsebeam_webrtc_sys::VideoBitrateAllocation {
            bandwidth_limited: true,
            ..Default::default()
        };
        result.layer_bitrates_bps[0][0] = Some(123_456);
        result
    }
    fn set_legacy_conference_mode(&mut self, enabled: bool) {
        self.0.legacy.lock().unwrap().push(enabled);
    }
}
impl Drop for AllocationProduct {
    fn drop(&mut self) {
        self.0.product_drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn per_peer_allocator_product_controls_real_native_encoder_rates_and_retains_environment() {
    use pulsebeam_webrtc_sys::{
        DataRate, PeerConnectionDependencies, VideoBitrateAllocatorFactory, VideoCodecType,
    };
    // Plain H264 initializes L1T1. FPS declarations do not enable another
    // native temporal mode; test a sparse single-temporal-layer allocation.
    let counters = Arc::new(Counters {
        encoder_fps_allocation: Some([vec![255], vec![], vec![], vec![], vec![]]),
        ..Default::default()
    });
    let observations = Arc::new(AllocationObservations::default());
    let registration = VideoBitrateAllocatorFactory::from_provider(AllocationFactory(
        observations.clone(),
        std::cell::Cell::new(0),
    ));
    let pair = Pair::new_with_dependencies(
        counters.clone(),
        false,
        None,
        h264(),
        PeerConnectionDependencies::default().video_bitrate_allocator_factory(&registration),
    );
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("custom-allocation", &source)
        .unwrap();
    let transceiver = pair
        .alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let codecs: Vec<_> = pair
        .alice
        .video_sender_capabilities()
        .unwrap()
        .into_iter()
        .filter(|codec| codec.format().name == "H264")
        .collect();
    transceiver.set_codec_preferences(&codecs).unwrap();
    drop(pair.negotiate());
    // A source's first frame can precede native encoder startup. Keep the
    // same finite progress budget while requiring real selected-product encode
    // work, rather than accepting only its pre-input rate callback.
    let observed = (0..20).any(|index| {
        source
            .push_frame(
                &VideoFrame::i420(
                    16,
                    16,
                    synthetic_i420(16, 16),
                    2_000_000 + i64::from(index) * 33_333,
                    90_000 + index * 3_000,
                )
                .unwrap(),
            )
            .unwrap();
        (0..100_000).any(|_| {
            pair.progress();
            counters.encode.load(Ordering::SeqCst) > 0
                && counters.encoder_rates.lock().unwrap().iter().any(|rates| {
                    rates.target_bitrate_bps == 123_456
                    && rates.target_layer_bitrates_bps[0][0] == Some(123_456)
                    && rates.target_layer_bitrates_bps[0][1].is_none()
                    && rates.target_layer_bitrates_bps[0][2].is_none()
                    && rates.target_bandwidth_limited
                    // Observe effective allocation independently. Native
                    // adjustment need not reduce this particular allocation.
                    && rates.bitrate_bps > 0
                    && rates.layer_bitrates_bps[0][0] == Some(rates.bitrate_bps)
                    && rates.layer_bitrates_bps[0][1].is_none()
                    && rates.layer_bitrates_bps[0][2].is_none()
                })
        })
    });
    assert!(
        observed,
        "native encoder did not consume the custom sparse allocation: encodes={}, creates={}, recent_rates={:?}, allocator_parameters={:?}",
        counters.encode.load(Ordering::SeqCst),
        counters.encoder_create.load(Ordering::SeqCst),
        counters
            .encoder_rates
            .lock()
            .unwrap()
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>(),
        observations
            .parameters
            .lock()
            .unwrap()
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
    );
    assert!(!registration.failed());
    let codecs = observations.codecs.lock().unwrap();
    assert!(!codecs.is_empty());
    assert!(
        codecs
            .iter()
            .all(|codec| codec.codec_type == VideoCodecType::H264
                && codec.width == 16
                && codec.height == 16)
    );
    assert!(
        codecs
            .iter()
            .all(|codec| codec.simulcast_streams.len() == 3 && codec.spatial_layers.len() == 5)
    );
    drop(codecs);
    assert!(observations.legacy.lock().unwrap().contains(&false));
    assert!(observations.parameters.lock().unwrap().iter().any(
        |parameters| matches!(parameters.total_bitrate, DataRate::BitsPerSecond(value) if value > 0)
            && parameters.framerate > 0.0
    ));
    let environment = observations
        .environment
        .lock()
        .unwrap()
        .take()
        .expect("factory environment");
    let timestamp = environment.timestamp();
    let trials = environment.field_trials();
    drop(registration);
    assert_eq!(observations.factory_drops.load(Ordering::SeqCst), 0);
    drop(transceiver);
    drop(track);
    drop(source);
    drop(pair);
    assert_eq!(observations.factory_drops.load(Ordering::SeqCst), 1);
    assert!(observations.product_drops.load(Ordering::SeqCst) > 0);
    assert_eq!(environment.timestamp(), timestamp);
    assert_eq!(
        environment
            .field_trials()
            .lookup("WebRTC-Bwe-InjectedCongestionController"),
        trials.lookup("WebRTC-Bwe-InjectedCongestionController")
    );
}

#[derive(Default)]
struct FecObservations {
    creates: AtomicUsize,
    factory_drops: AtomicUsize,
    product_drops: AtomicUsize,
    environment: Mutex<Option<pulsebeam_webrtc_sys::FecControllerEnvironment>>,
    callback: Mutex<Option<pulsebeam_webrtc_sys::FecProtectionCallback>>,
    methods: Mutex<Vec<(bool, bool)>>,
    encoding: Mutex<Vec<pulsebeam_webrtc_sys::FecEncodingData>>,
    encoded: Mutex<Vec<(usize, pulsebeam_webrtc_sys::FecEncodedFrameType)>>,
    rates: Mutex<Vec<pulsebeam_webrtc_sys::FecRateUpdate>>,
    async_started: std::sync::atomic::AtomicBool,
    protection: Mutex<
        Option<
            pulsebeam_webrtc_sys::FecCallbackOperation<pulsebeam_webrtc_sys::FecProtectionResponse>,
        >,
    >,
    retransmission: Mutex<Option<pulsebeam_webrtc_sys::FecCallbackOperation<()>>>,
}
struct FecFactory(Arc<FecObservations>, std::cell::Cell<usize>);
impl pulsebeam_webrtc_sys::FecControllerFactoryProvider for FecFactory {
    fn create_fec_controller(
        &mut self,
        environment: pulsebeam_webrtc_sys::FecControllerEnvironment,
    ) -> pulsebeam_webrtc_sys::FecControllerCreation {
        self.1.set(self.1.get() + 1);
        self.0.creates.fetch_add(1, Ordering::SeqCst);
        *self.0.environment.lock().unwrap() = Some(environment);
        pulsebeam_webrtc_sys::FecControllerCreation::Provider(Box::new(FecProduct(self.0.clone())))
    }
}
impl Drop for FecFactory {
    fn drop(&mut self) {
        self.0.factory_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct FecProduct(Arc<FecObservations>);
impl pulsebeam_webrtc_sys::FecController for FecProduct {
    fn set_protection_callback(
        &self,
        callback: Option<pulsebeam_webrtc_sys::FecProtectionCallback>,
    ) {
        *self.0.callback.lock().unwrap() = callback;
    }
    fn set_protection_method(&self, fec: bool, nack: bool) {
        self.0.methods.lock().unwrap().push((fec, nack));
    }
    fn set_encoding_data(&self, data: pulsebeam_webrtc_sys::FecEncodingData) {
        self.0.encoding.lock().unwrap().push(data);
    }
    fn update_fec_rates(&self, update: pulsebeam_webrtc_sys::FecRateUpdate) -> u32 {
        use pulsebeam_webrtc_sys::{
            FecProtectionCallbackError, FecProtectionParams, FecRetransmissionMode,
        };
        let callback = self
            .0
            .callback
            .lock()
            .unwrap()
            .clone()
            .expect("native callback installed");
        // This is the SDK's synchronous protection callback path, on worker.
        callback
            .request(
                FecProtectionParams::default(),
                FecProtectionParams::default(),
            )
            .unwrap();
        assert_eq!(
            callback.set_retransmission_mode(FecRetransmissionMode::ALL_LAYERS),
            Err(FecProtectionCallbackError::ReentrantNativeLock)
        );
        if !self.0.async_started.swap(true, Ordering::SeqCst) {
            *self.0.protection.lock().unwrap() = Some(callback.request_async(
                FecProtectionParams::default(),
                FecProtectionParams::default(),
            ));
            *self.0.retransmission.lock().unwrap() =
                Some(callback.set_retransmission_mode_async(FecRetransmissionMode::ALL_LAYERS));
        }
        let rate = update.estimated_bitrate_bps;
        self.0.rates.lock().unwrap().push(update);
        rate
    }
    fn update_with_encoded_data(
        &self,
        length: usize,
        frame_type: pulsebeam_webrtc_sys::FecEncodedFrameType,
    ) {
        self.0.encoded.lock().unwrap().push((length, frame_type));
    }
    fn use_loss_vector_mask(&self) -> bool {
        true
    }
}
impl Drop for FecProduct {
    fn drop(&mut self) {
        self.0.product_drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct NetworkControlObservations {
    creates: AtomicUsize,
    factory_drops: AtomicUsize,
    product_drops: AtomicUsize,
    callbacks: std::sync::Mutex<Vec<&'static str>>,
    environment: std::sync::Mutex<Option<pulsebeam_webrtc_sys::NetworkControllerEnvironment>>,
}
struct NetworkControlFactory(Arc<NetworkControlObservations>, std::cell::Cell<u32>);
struct NetworkControlProduct(Arc<NetworkControlObservations>, std::cell::Cell<u32>);
impl Drop for NetworkControlFactory {
    fn drop(&mut self) {
        self.0.factory_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Drop for NetworkControlProduct {
    fn drop(&mut self) {
        self.0.product_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl pulsebeam_webrtc_sys::NetworkControllerFactoryProvider for NetworkControlFactory {
    fn create(
        &mut self,
        config: pulsebeam_webrtc_sys::NetworkControllerConfig,
    ) -> Box<dyn pulsebeam_webrtc_sys::NetworkController> {
        self.1.set(self.1.get() + 1);
        assert!(
            config
                .environment
                .field_trials()
                .is_enabled("WebRTC-Bwe-InjectedCongestionController")
        );
        assert_eq!(
            config.default_pacing_time_window,
            pulsebeam_webrtc_sys::PacerConfig::DEFAULT_TIME_WINDOW
        );
        self.0.creates.fetch_add(1, Ordering::SeqCst);
        *self.0.environment.lock().unwrap() = Some(config.environment);
        Box::new(NetworkControlProduct(
            self.0.clone(),
            std::cell::Cell::new(0),
        ))
    }
    fn process_interval(&mut self) -> pulsebeam_webrtc_sys::TimeDelta {
        pulsebeam_webrtc_sys::TimeDelta::from_native_microsecond_bits(25_000)
    }
}
impl NetworkControlProduct {
    fn update(
        &mut self,
        method: &'static str,
        at_time: pulsebeam_webrtc_sys::Timestamp,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        use pulsebeam_webrtc_sys::{
            DataRate, NetworkControlUpdate, NetworkEstimate, PacerConfig, TargetTransferRate,
            TimeDelta,
        };
        self.1.set(self.1.get() + 1);
        self.0.callbacks.lock().unwrap().push(method);
        let at_time = if at_time.is_finite() {
            at_time
        } else {
            self.0
                .environment
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .timestamp()
        };
        NetworkControlUpdate {
            pacer_config: Some(
                PacerConfig::create(
                    at_time,
                    DataRate::BitsPerSecond(2_000_000),
                    DataRate::BitsPerSecond(0),
                    PacerConfig::DEFAULT_TIME_WINDOW,
                )
                .unwrap(),
            ),
            target_rate: Some(TargetTransferRate {
                at_time,
                target_rate: DataRate::BitsPerSecond(1_500_000),
                network_estimate: NetworkEstimate {
                    at_time,
                    bandwidth: DataRate::BitsPerSecond(1_500_000),
                    round_trip_time: TimeDelta::from_native_microsecond_bits(10_000),
                    bwe_period: TimeDelta::from_native_microsecond_bits(25_000),
                    loss_rate_ratio: 0.0,
                },
                cwnd_reduce_ratio: 0.0,
            }),
            ..Default::default()
        }
    }
}
impl pulsebeam_webrtc_sys::NetworkController for NetworkControlProduct {
    fn on_network_availability(
        &mut self,
        value: pulsebeam_webrtc_sys::NetworkAvailability,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("network_availability", value.at_time)
    }
    fn on_network_route_change(
        &mut self,
        value: pulsebeam_webrtc_sys::NetworkRouteChange,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("network_route_change", value.at_time)
    }
    fn on_process_interval(
        &mut self,
        value: pulsebeam_webrtc_sys::ProcessInterval,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("process_interval", value.at_time)
    }
    fn on_remote_bitrate_report(
        &mut self,
        value: pulsebeam_webrtc_sys::RemoteBitrateReport,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("remote_bitrate_report", value.receive_time)
    }
    fn on_round_trip_time_update(
        &mut self,
        value: pulsebeam_webrtc_sys::RoundTripTimeUpdate,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("round_trip_time_update", value.receive_time)
    }
    fn on_sent_packet(
        &mut self,
        value: pulsebeam_webrtc_sys::SentPacket,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("sent_packet", value.send_time)
    }
    fn on_received_packet(
        &mut self,
        value: pulsebeam_webrtc_sys::NetworkControlReceivedPacket,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("received_packet", value.receive_time)
    }
    fn on_streams_config(
        &mut self,
        value: pulsebeam_webrtc_sys::StreamsConfig,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("streams_config", value.at_time)
    }
    fn on_target_rate_constraints(
        &mut self,
        value: pulsebeam_webrtc_sys::TargetRateConstraints,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("target_rate_constraints", value.at_time)
    }
    fn on_transport_loss_report(
        &mut self,
        value: pulsebeam_webrtc_sys::TransportLossReport,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("transport_loss_report", value.receive_time)
    }
    fn on_transport_packets_feedback(
        &mut self,
        value: pulsebeam_webrtc_sys::TransportPacketsFeedback,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("transport_packets_feedback", value.feedback_time)
    }
    fn on_network_state_estimate(
        &mut self,
        value: pulsebeam_webrtc_sys::NetworkStateEstimate,
    ) -> pulsebeam_webrtc_sys::NetworkControlUpdate {
        self.update("network_state_estimate", value.update_time)
    }
    fn supports_ecn_adaptation(&mut self) -> bool {
        true
    }
}
#[test]
fn injected_network_controller_drives_native_video_without_an_additional_owner() {
    use pulsebeam_webrtc_sys::NetworkControllerFactory;
    let counters = Arc::new(Counters::default());
    let observations = Arc::new(NetworkControlObservations::default());
    let registration = NetworkControllerFactory::from_provider(NetworkControlFactory(
        observations.clone(),
        std::cell::Cell::new(0),
    ));
    let pair = Pair::new_with_call_factories(
        counters.clone(),
        false,
        None,
        h264(),
        Default::default(),
        None,
        Some(&registration),
    );
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("custom-network-control", &source)
        .unwrap();
    let transceiver = pair
        .alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let codecs: Vec<_> = pair
        .alice
        .video_sender_capabilities()
        .unwrap()
        .into_iter()
        .filter(|codec| codec.format().name == "H264")
        .collect();
    transceiver.set_codec_preferences(&codecs).unwrap();
    drop(pair.negotiate());
    // Native encoder/pacer work is asynchronous and may discard a startup
    // frame. Feed at camera cadence instead of racing millions of simulated
    // milliseconds past real worker timers after submitting just one frame.
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(15);
    let mut next_frame = started;
    let mut frame_index = 0u32;
    let observed = loop {
        let now = std::time::Instant::now();
        if now >= next_frame {
            source
                .push_frame(
                    &VideoFrame::i420(
                        16,
                        16,
                        synthetic_i420(16, 16),
                        2_000_000 + i64::from(frame_index) * 33_000,
                        90_000 + frame_index * 2_970,
                    )
                    .unwrap(),
                )
                .unwrap();
            frame_index += 1;
            next_frame = now + Duration::from_millis(33);
        }
        pair.progress();
        if counters.encode.load(Ordering::SeqCst) > 0
            && observations
                .callbacks
                .lock()
                .unwrap()
                .contains(&"sent_packet")
        {
            break true;
        }
        if now >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(1));
    };
    assert!(
        observed,
        "injected native controller did not encode/send: encodes={} callbacks={:?}",
        counters.encode.load(Ordering::SeqCst),
        observations.callbacks.lock().unwrap().as_slice(),
    );
    assert!(!registration.has_failed());
    assert!(observations.creates.load(Ordering::SeqCst) > 0);
    assert!(
        observations
            .callbacks
            .lock()
            .unwrap()
            .contains(&"network_availability")
    );
    let retained = observations.environment.lock().unwrap().take().unwrap();
    let trials = retained.field_trials();
    let timestamp = retained.timestamp();
    drop(registration);
    assert_eq!(observations.factory_drops.load(Ordering::SeqCst), 0);
    drop(transceiver);
    drop(track);
    drop(source);
    drop(pair);
    assert_eq!(observations.factory_drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        observations.product_drops.load(Ordering::SeqCst),
        observations.creates.load(Ordering::SeqCst)
    );
    assert_eq!(retained.timestamp(), timestamp);
    assert_eq!(
        retained
            .field_trials()
            .lookup("WebRTC-Bwe-InjectedCongestionController"),
        trials.lookup("WebRTC-Bwe-InjectedCongestionController")
    );
}

#[test]
fn factory_fec_product_uses_native_protection_callback_and_existing_worker_dispatch() {
    use pulsebeam_webrtc_sys::{
        FecControllerFactory, FecEncodedFrameType, FecProtectionCallbackError, FecProtectionParams,
    };
    let counters = Arc::new(Counters::default());
    let observations = Arc::new(FecObservations::default());
    let registration = FecControllerFactory::from_provider(FecFactory(
        observations.clone(),
        std::cell::Cell::new(0),
    ));
    let pair = Pair::new_with_fec(
        counters.clone(),
        false,
        None,
        h264(),
        Default::default(),
        Some(&registration),
    );
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("custom-fec", &source)
        .unwrap();
    let transceiver = pair
        .alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let codecs: Vec<_> = pair
        .alice
        .video_sender_capabilities()
        .unwrap()
        .into_iter()
        .filter(|codec| codec.format().name == "H264")
        .collect();
    transceiver.set_codec_preferences(&codecs).unwrap();
    drop(pair.negotiate());
    source
        .push_frame(&VideoFrame::i420(16, 16, synthetic_i420(16, 16), 2_000_000, 90_000).unwrap())
        .unwrap();
    let mut protection = None;
    let mut retransmission = None;
    let observed = (0..2_000_000).any(|_| {
        pair.progress();
        if protection.is_none() {
            protection = observations
                .protection
                .lock()
                .unwrap()
                .as_mut()
                .and_then(|operation| operation.try_take());
        }
        if retransmission.is_none() {
            retransmission = observations
                .retransmission
                .lock()
                .unwrap()
                .as_mut()
                .and_then(|operation| operation.try_take());
        }
        counters.encode.load(Ordering::SeqCst) > 0
            && !observations.encoded.lock().unwrap().is_empty()
            && protection.is_some()
            && retransmission.is_some()
    });
    assert!(
        observed,
        "native FEC callback or queued operation was not consumed"
    );
    assert_eq!(protection.unwrap().unwrap().native_result, 0);
    assert_eq!(retransmission, Some(Ok(())));
    assert!(!registration.has_failed());
    assert!(observations.creates.load(Ordering::SeqCst) > 0);
    assert!(!observations.methods.lock().unwrap().is_empty());
    assert!(
        observations
            .encoding
            .lock()
            .unwrap()
            .iter()
            .any(|data| data.width == 16 && data.height == 16)
    );
    assert!(
        observations
            .encoded
            .lock()
            .unwrap()
            .iter()
            .any(|(length, frame_type)| *length > 0 && *frame_type == FecEncodedFrameType::Key)
    );
    assert!(!observations.rates.lock().unwrap().is_empty());
    let callback = observations.callback.lock().unwrap().clone().unwrap();
    // Affinity is not sufficient outside a known live native invocation.
    assert_eq!(
        callback.request(
            FecProtectionParams::default(),
            FecProtectionParams::default()
        ),
        Err(FecProtectionCallbackError::OutsideInvocation)
    );
    let environment = observations.environment.lock().unwrap().take().unwrap();
    let timestamp = environment.timestamp();
    let trials = environment.field_trials();
    drop(registration);
    assert_eq!(observations.factory_drops.load(Ordering::SeqCst), 0);
    drop(transceiver);
    drop(track);
    drop(source);
    drop(pair);
    assert_eq!(observations.factory_drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        observations.product_drops.load(Ordering::SeqCst),
        observations.creates.load(Ordering::SeqCst)
    );
    assert_eq!(
        callback
            .request_async(
                FecProtectionParams::default(),
                FecProtectionParams::default()
            )
            .try_take(),
        Some(Err(FecProtectionCallbackError::Retired))
    );
    assert_eq!(environment.timestamp(), timestamp);
    assert_eq!(
        environment
            .field_trials()
            .lookup("WebRTC-Bwe-InjectedCongestionController"),
        trials.lookup("WebRTC-Bwe-InjectedCongestionController")
    );
}

#[test]
fn injected_h264_provider_carries_a_frame_between_peers() {
    let counters = Arc::new(Counters::default());
    let pair = Pair::new(counters.clone(), false);
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("synthetic", &source)
        .unwrap();
    let local_sink = track.attach_sink().unwrap();
    let transceiver = pair
        .alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    assert_eq!(transceiver.direction(), RtpTransceiverDirection::SendOnly);
    assert_eq!(transceiver.sender().track().unwrap().id(), "synthetic");
    assert!(transceiver.sender().dtmf_sender().is_none());
    assert_eq!(
        transceiver.sender().media_type(),
        pulsebeam_webrtc_sys::MediaType::Video
    );
    assert_eq!(
        transceiver.sender().init_send_encodings(),
        vec![pulsebeam_webrtc_sys::RtpEncodingParameters::default()]
    );

    let sender_codecs = pair.alice.video_sender_capabilities().unwrap();
    let receiver_codecs = pair.bob.video_receiver_capabilities().unwrap();
    let h264_codecs: Vec<_> = sender_codecs
        .into_iter()
        .filter(|codec| {
            codec.format().name == "H264"
                && codec
                    .format()
                    .parameters
                    .iter()
                    .any(|param| param.key == "packetization-mode" && param.value == "1")
        })
        .collect();
    assert!(
        !h264_codecs.is_empty(),
        "actual H264 sender capability missing"
    );
    assert!(
        receiver_codecs
            .iter()
            .any(|codec| codec.format().name == "H264")
    );
    assert_eq!(h264_codecs[0].clock_rate(), Some(90_000));
    assert!(
        transceiver
            .set_codec_preferences(&[h264_codecs[0].clone(), h264_codecs[0].clone(),])
            .is_err(),
        "duplicate capability must be rejected"
    );
    transceiver.set_codec_preferences(&h264_codecs).unwrap();

    let mut bob_events = pair.negotiate();
    let remote = (0..2_000_000)
        .find_map(|_| {
            if let Some(remote) = take_remote_transceiver(&mut bob_events) {
                return Some(remote);
            }
            bob_events.extend(pair.progress().1);
            None
        })
        .expect("remote video transceiver did not arrive");
    // TrackAdded also owns a receiver/peer. Retire unused observation handles
    // before asserting final native codec destruction below.
    drop(bob_events);
    assert_eq!(
        remote.current_direction(),
        Some(RtpTransceiverDirection::ReceiveOnly)
    );
    let receiver = remote.receiver();
    receiver.request_keyframe().unwrap();
    let remote_track = receiver.track().unwrap();
    let sink = remote_track.attach_sink().unwrap();

    for _ in 0..100_000 {
        pair.progress();
        if transceiver.current_direction() == Some(RtpTransceiverDirection::SendOnly) {
            break;
        }
    }
    let packed = VideoFrame::i420(16, 16, synthetic_i420(16, 16), 2_000_000, 90_000).unwrap();
    let frame = VideoFrame::nv12(16, 16, packed.to_nv12().unwrap(), 2_000_000, 90_000)
        .unwrap()
        .with_rotation(VideoRotation::Clockwise90);
    source.push_frame(&frame).unwrap();
    assert_eq!(
        local_sink
            .try_next_frame()
            .expect("caller-fed source did not preserve the local frame"),
        frame
    );
    let received = (0..2_000_000)
        .find_map(|_| {
            pair.progress();
            sink.try_next_frame()
        })
        .unwrap_or_else(|| {
            panic!(
                "video frame did not arrive: encoders={} encoded={} decoders={} decoded={}",
                counters.encoder_create.load(Ordering::SeqCst),
                counters.encode.load(Ordering::SeqCst),
                counters.decoder_create.load(Ordering::SeqCst),
                counters.decode.load(Ordering::SeqCst),
            )
        });
    assert_eq!((received.width, received.height), (16, 16));
    assert_ne!(received.rtp_timestamp, 0);
    assert_eq!(
        counters.decoder_rtp_timestamp.load(Ordering::SeqCst),
        received.rtp_timestamp
    );
    assert_eq!(received.buffer.as_bytes(), synthetic_i420(16, 16));
    assert_eq!(received.to_nv12().unwrap(), frame.to_nv12().unwrap());
    let sender = transceiver.sender();
    assert_eq!(
        sender
            .request_keyframe(&["unknown-rid".to_string()])
            .unwrap_err()
            .kind,
        PeerErrorKind::InvalidParameter,
    );
    sender.request_keyframe(&[]).unwrap();
    assert!(counters.encoder_factory.load(Ordering::SeqCst) > 0);
    assert!(counters.decoder_factory.load(Ordering::SeqCst) > 0);
    assert_eq!(counters.encoder_create.load(Ordering::SeqCst), 1);
    assert_eq!(counters.decoder_create.load(Ordering::SeqCst), 1);
    assert_eq!(counters.encode.load(Ordering::SeqCst), 1);
    let initialized = counters.encoder_settings.lock().unwrap();
    assert!(!initialized.is_empty());
    assert!(
        initialized
            .iter()
            .all(|settings| settings.h264_temporal_layers == Some(1)
                && settings.simulcast_temporal_layers == [1])
    );
    for settings in initialized.iter() {
        assert_eq!(
            settings.simulcast_streams,
            [VideoSimulcastStream {
                width: 16,
                height: 16,
                active: true,
                temporal_layers: 1,
            }]
        );
    }
    drop(initialized);
    assert_eq!(counters.encoder_rotation.load(Ordering::SeqCst), 90);
    assert_eq!(counters.decode.load(Ordering::SeqCst), 1);

    let mut encoded_sink = receiver.attach_encoded_sink().unwrap();
    assert!(receiver.attach_encoded_sink().is_err());
    let second = VideoFrame::i420(16, 16, synthetic_i420(16, 16), 3_000_000, 180_000).unwrap();
    source.push_frame(&second).unwrap();
    let encoded = (0..2_000_000)
        .find_map(|_| {
            pair.progress();
            encoded_sink.try_next_frame()
        })
        .expect("encoded receive frame was not intercepted");
    assert!(encoded.data.starts_with(&[0, 0, 0, 1, 0x67]));
    assert!(encoded.mime_type.eq_ignore_ascii_case("video/H264"));
    assert_ne!(encoded.ssrc, 0);
    assert!(encoded.key_frame);
    assert_eq!(encoded.spatial_index.is_some(), encoded.frame_id.is_some());
    assert_eq!(encoded.temporal_index.is_some(), encoded.frame_id.is_some());
    assert_eq!(counters.decode.load(Ordering::SeqCst), 1);
    assert_eq!(encoded_sink.dropped_frames(), 0);
    encoded_sink.close();
    assert!(encoded_sink.try_next_frame().is_none());
    assert!(receiver.attach_encoded_sink().is_err());
    let third = VideoFrame::i420(16, 16, synthetic_i420(16, 16), 4_000_000, 270_000).unwrap();
    source.push_frame(&third).unwrap();
    let resumed = (0..2_000_000)
        .find_map(|_| {
            pair.progress();
            sink.try_next_frame()
        })
        .expect("decoded receive did not resume after closing encoded sink");
    assert_eq!((resumed.width, resumed.height), (16, 16));
    assert_eq!(counters.decode.load(Ordering::SeqCst), 2);
    drop(encoded_sink);
    drop(sink);
    drop(remote_track);
    drop(receiver);
    drop(remote);
    drop(local_sink);
    drop(sender);
    drop(transceiver);
    drop(track);
    drop(source);
    drop(pair);
    // SDK media-engine destruction can be posted to its worker queue.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while counters.encoder_release.load(Ordering::SeqCst) == 0
        || counters.decoder_release.load(Ordering::SeqCst) == 0
    {
        assert!(
            std::time::Instant::now() < deadline,
            "codec teardown did not retire"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(counters.encoder_release.load(Ordering::SeqCst), 1);
    assert_eq!(counters.decoder_release.load(Ordering::SeqCst), 1);
}

#[test]
fn sender_replaces_and_detaches_track_without_replacing_transceiver() {
    let counters = Arc::new(Counters::default());
    let pair = Pair::new(counters.clone(), false);
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("first", &source)
        .unwrap();
    let transceiver = pair
        .alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let sender = transceiver.sender();
    let id = sender.id();
    drop(track);
    drop(source);
    assert_eq!(sender.track().unwrap().id(), "first");
    assert_eq!(sender.track().unwrap().state(), VideoTrackState::Live);
    let mut bob_events = pair.negotiate();
    let remote = (0..2_000_000)
        .find_map(|_| {
            if let Some(remote) = take_remote_transceiver(&mut bob_events) {
                return Some(remote);
            }
            bob_events.extend(pair.progress().1);
            None
        })
        .expect("remote video transceiver did not arrive");
    let remote_track = remote.receiver().track().unwrap();
    let sink = remote_track.attach_sink().unwrap();
    assert!(sender.set_track(Some(&remote_track)).is_err());
    assert_eq!(sender.track().unwrap().id(), "first");

    let replacement_source = pair.alice_factory.create_video_source().unwrap();
    let replacement = pair
        .alice_factory
        .create_video_track("replacement", &replacement_source)
        .unwrap();
    sender.set_track(Some(&replacement)).unwrap();
    drop(replacement);
    assert_eq!(sender.id(), id);
    assert_eq!(sender.track().unwrap().id(), "replacement");
    let frame = VideoFrame::i420(32, 16, synthetic_i420(32, 16), 2_000_000, 90_000).unwrap();
    for _ in 0..100_000 {
        pair.progress();
        if transceiver.current_direction() == Some(RtpTransceiverDirection::SendOnly) {
            break;
        }
    }
    replacement_source.push_frame(&frame).unwrap();
    let received = (0..2_000_000)
        .find_map(|_| {
            pair.progress();
            sink.try_next_frame()
        })
        .expect("replacement source frame did not arrive");
    // The fixture's fixed H.264 SPS advertises 16x16 even for a 32x16 input;
    // the original source has never pushed a frame, so this arrived after swap.
    assert_eq!((received.width, received.height), (16, 16));
    assert!(counters.encode.load(Ordering::SeqCst) > 0);
    sender.set_track(None).unwrap();
    assert!(sender.track().is_none());
    assert_eq!(sender.id(), id);
    assert_eq!(pair.alice.video_transceivers().unwrap().len(), 1);
}

#[test]
fn video_transceiver_snapshot_mid_and_stop_follow_negotiation() {
    let counters = Arc::new(Counters::default());
    let mut pair = Pair::new(counters, false);
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("enumerated", &source)
        .unwrap();
    let transceiver = pair
        .alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    assert_eq!(pair.alice.video_transceivers().unwrap().len(), 1);
    assert_eq!(pair.alice.video_senders().unwrap().len(), 1);
    assert_eq!(pair.alice.video_receivers().unwrap().len(), 1);
    assert_eq!(transceiver.mid(), None);
    pair.negotiate();
    let mid = transceiver.mid().expect("negotiated video MID");
    let local = pair.alice.video_transceivers().unwrap();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].mid().as_deref(), Some(mid.as_str()));
    assert_eq!(local[0].sender().track().unwrap().id(), "enumerated");
    assert_eq!(
        pair.alice.video_senders().unwrap()[0].id(),
        transceiver.sender().id()
    );
    let remote = pair.bob.video_transceivers().unwrap();
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].mid().as_deref(), Some(mid.as_str()));
    assert_eq!(pair.bob.video_receivers().unwrap().len(), 1);
    transceiver.stop().unwrap();
    // Stopping without an ICE restart reuses the completed gathering generation.
    // Exchange the resulting gathered SDP; no per-candidate forwarding occurs.
    let mut alice_events = Vec::new();
    let mut bob_events = Vec::new();
    let offer = completion_description(
        &pair.alice,
        pair.alice.create_offer().unwrap(),
        &mut alice_events,
    );
    completion(
        &pair.alice,
        pair.alice.set_local_description(offer).unwrap(),
        &mut alice_events,
    );
    let gathered = pair.alice.descriptions().unwrap().pending_local.unwrap();
    assert!(gathered.sdp.contains("a=candidate:"));
    completion(
        &pair.bob,
        pair.bob.set_remote_description(gathered).unwrap(),
        &mut bob_events,
    );
    let answer = completion_description(
        &pair.bob,
        pair.bob.create_answer().unwrap(),
        &mut bob_events,
    );
    completion(
        &pair.bob,
        pair.bob.set_local_description(answer).unwrap(),
        &mut bob_events,
    );
    let gathered = pair.bob.descriptions().unwrap().current_local.unwrap();
    assert!(gathered.sdp.contains("a=candidate:"));
    completion(
        &pair.alice,
        pair.alice.set_remote_description(gathered).unwrap(),
        &mut alice_events,
    );
    assert!(transceiver.stopped());
    // Upstream removes a fully stopped transceiver from enumeration; an
    // existing owned handle remains readable for the final state.
    assert!(pair.alice.video_transceivers().unwrap().is_empty());
    assert!(pair.alice.video_senders().unwrap().is_empty());
    assert!(pair.alice.video_receivers().unwrap().is_empty());
    pair.alice.close().unwrap();
    assert!(pair.alice.video_transceivers().is_err());
    assert!(transceiver.stop().is_err());
}

#[test]
fn video_rids_are_validated_and_fixed_before_negotiation() {
    let counters = Arc::new(Counters::default());
    let mut pair = Pair::new(counters.clone(), false);
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("rid-track", &source)
        .unwrap();
    for rids in [
        vec!["f".into(), "f".into()],
        vec!["".into()],
        vec!["rid-too-long-for-rtp".into()],
        vec!["bad rid".into()],
    ] {
        assert_eq!(
            pair.alice
                .add_video_transceiver_with_rids(&track, RtpTransceiverDirection::SendOnly, &rids,)
                .unwrap_err()
                .kind,
            PeerErrorKind::InvalidParameter,
        );
    }
    assert!(pair.alice.video_transceivers().unwrap().is_empty());
    let transceiver = pair
        .alice
        .add_video_transceiver_with_rids(
            &track,
            RtpTransceiverDirection::SendOnly,
            &["f".into(), "h".into()],
        )
        .unwrap();
    let sender = transceiver.sender();
    let snapshot = sender.parameters().unwrap();
    assert_eq!(
        snapshot
            .encodings
            .iter()
            .map(|entry| entry.rid.as_str())
            .collect::<Vec<_>>(),
        vec!["f", "h"]
    );
    assert!(snapshot.encodings.iter().all(|entry| entry.active));
    let mut configured = snapshot.clone();
    configured.encodings[0].scale_resolution_down_by = Some(8.0);
    configured.encodings[1].scale_resolution_down_by = Some(4.0);
    sender.set_parameters(configured).unwrap();
    assert_eq!(pair.alice.video_transceivers().unwrap().len(), 1);
    let mut bob_events = pair.negotiate_with_receive_rids(true);
    let remote = (0..2_000_000)
        .find_map(|_| {
            if let Some(transceiver) = take_remote_transceiver(&mut bob_events) {
                return Some(transceiver);
            }
            bob_events.extend(pair.progress().1);
            None
        })
        .expect("simulcast receiver missing");
    let mut encoded = remote.receiver().attach_encoded_sink().unwrap();
    let negotiated = transceiver.sender().parameters().unwrap();
    assert_eq!(
        negotiated
            .encodings
            .iter()
            .map(|e| e.rid.as_str())
            .collect::<Vec<_>>(),
        vec!["f", "h"],
        "remote={:?}",
        pair.alice
            .descriptions()
            .unwrap()
            .current_remote
            .unwrap()
            .sdp
            .lines()
            .filter(|line| line.contains("rid")
                || line.contains("simulcast")
                || line.contains("extmap:"))
            .collect::<Vec<_>>()
    );
    // This is a threaded SDK fixture, not a managed-execution claim. Its
    // native queues run on elapsed time. Advancing the injected media clock
    // 100 seconds per input frame outruns those queues and the SDK's two-second
    // encoder inactivity timer. Keep this clock aligned with elapsed time,
    // including capture timestamps, while the existing workers run naturally.
    let started = Instant::now();
    let clock_origin = pair.clock.now();
    let progress = || {
        let target = clock_origin + started.elapsed();
        pair.clock
            .advance(target.saturating_sub(pair.clock.now()))
            .unwrap();
        let events = (drain(&pair.alice), drain(&pair.bob));
        while let Some(packet) = pair.network.next_packet() {
            pair.network.deliver(packet.id).unwrap();
        }
        thread::yield_now();
        events
    };
    let mut ssrcs = std::collections::HashSet::new();
    for index in 0..12 {
        progress();
        source
            .push_frame(
                &VideoFrame::i420(
                    640,
                    360,
                    synthetic_i420(640, 360),
                    pair.clock.now().as_micros() as i64,
                    index as u32,
                )
                .unwrap(),
            )
            .unwrap();
        let next_capture = started.elapsed() + Duration::from_micros(33_333);
        loop {
            progress();
            while let Some(frame) = encoded.try_next_frame() {
                ssrcs.insert(frame.ssrc);
            }
            if ssrcs.len() == 2 || started.elapsed() >= next_capture {
                break;
            }
        }
        if ssrcs.len() == 2 {
            break;
        }
    }
    assert!(
        counters.encoder_create.load(Ordering::SeqCst) >= 2,
        "simulcast must instantiate one encoder per layer"
    );
    assert!(
        !ssrcs.is_empty(),
        "at least one layer must arrive at the peer receiver: native encodes={}, clock={:?}, settings={:?}, rates={:?}",
        counters.encode.load(Ordering::SeqCst),
        pair.clock.now(),
        counters.encoder_settings.lock().unwrap(),
        counters.encoder_rates.lock().unwrap(),
    );
    let operation = pair.alice.request_stats().unwrap();
    let snapshot = (0..100_000)
        .find_map(|_| {
            progress().0.into_iter().find_map(|event| match event {
                PeerConnectionEvent::Stats(stats) if stats.operation_id == operation => Some(stats),
                _ => None,
            })
        })
        .expect("sender stats missing");
    let outgoing = snapshot
        .records
        .iter()
        .filter_map(|record| match record {
            PeerStatsRecord::OutboundRtp(stats)
                if stats.kind.as_deref() == Some(b"video".as_slice()) =>
            {
                Some((stats.rid.as_deref(), stats.ssrc, stats.packets_sent))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        outgoing
            .iter()
            .filter_map(|(rid, _, _)| *rid)
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([b"f".as_slice(), b"h".as_slice()]),
        "each simulcast RID needs an outbound stream: {outgoing:?}"
    );
    assert_eq!(
        outgoing
            .iter()
            .filter_map(|(_, ssrc, _)| *ssrc)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        2,
        "simulcast layers must use distinct SSRCs: {outgoing:?}"
    );
    assert!(
        outgoing
            .iter()
            .all(|(_, _, packets)| packets.unwrap_or(0) > 0),
        "both layers must send RTP: {outgoing:?}"
    );
    encoded.close();
    pair.alice.close().unwrap();
    pair.bob.close().unwrap();
}

#[test]
fn full_native_keyframe_operation_preserves_noop_and_rid_error_order() {
    let pair = Pair::new(Arc::new(Counters::default()), false);
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("native-keyframe", &source)
        .unwrap();
    let transceiver = pair
        .alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let sender = transceiver.sender();
    assert_eq!(sender.ssrc(), 0);
    // The actual SDK succeeds before validating RIDs without an active channel.
    sender.generate_keyframe(&[vec![], vec![0xff, 0]]).unwrap();
    pair.negotiate();
    let error = sender.generate_keyframe(&[vec![]]).unwrap_err();
    let RtpSenderOperationError::Native(error) = error else {
        panic!("actual native RID error required")
    };
    assert_eq!(error.kind(), RtcErrorType::InvalidParameter);
    assert_eq!(error.message_bytes(), b"Attempted to specify an empty rid.");
    let error = sender.generate_keyframe(&[vec![0xff, 0]]).unwrap_err();
    let RtpSenderOperationError::Native(error) = error else {
        panic!("actual native RID error required")
    };
    assert_eq!(error.kind(), RtcErrorType::InvalidParameter);
    assert_eq!(
        error.message_bytes(),
        b"Attempted to specify a rid not configured."
    );
    transceiver.stop_standard().unwrap();
    sender.generate_keyframe(&[vec![], vec![0xff, 0]]).unwrap();
}

#[test]
fn sender_encoding_updates_preserve_transaction_and_validate_values() {
    let counters = Arc::new(Counters::default());
    let pair = Pair::new(counters, false);
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("params", &source)
        .unwrap();
    let transceiver = pair
        .alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    pair.negotiate();
    let sender = transceiver.sender();
    let mut snapshot = sender.parameters().unwrap();
    assert_eq!(snapshot.encodings.len(), 1);
    let stale = snapshot.clone();
    snapshot.encodings[0].max_bitrate_bps = Some(150_000);
    snapshot.encodings[0].scale_resolution_down_by = Some(2.0);
    snapshot.encodings[0].scale_resolution_down_to = Some(VideoResolution {
        width: 8,
        height: 8,
    });
    snapshot.encodings[0].max_framerate = Some(24.0);
    snapshot.encodings[0].active = false;
    sender.set_parameters(snapshot).unwrap();
    let current = sender.parameters().unwrap();
    assert_eq!(current.encodings[0].max_bitrate_bps, Some(150_000));
    assert_eq!(current.encodings[0].scale_resolution_down_by, Some(2.0));
    assert_eq!(
        current.encodings[0].scale_resolution_down_to,
        Some(VideoResolution {
            width: 8,
            height: 8
        })
    );
    assert_eq!(current.encodings[0].max_framerate, Some(24.0));
    assert!(!current.encodings[0].active);
    assert!(sender.set_parameters(stale).is_err());

    let mut invalid = sender.parameters().unwrap();
    invalid.encodings[0].scale_resolution_down_by = Some(f64::NAN);
    assert!(sender.set_parameters(invalid).is_err());
    let mut invalid = sender.parameters().unwrap();
    invalid.encodings[0].scale_resolution_down_to = Some(VideoResolution {
        width: 0,
        height: 8,
    });
    assert!(sender.set_parameters(invalid).is_err());
    let mut invalid = sender.parameters().unwrap();
    invalid.encodings[0].rid = "changed".into();
    assert!(sender.set_parameters(invalid).is_err());
    let mut invalid = sender.parameters().unwrap();
    invalid.encodings[0].max_framerate = Some((i32::MAX as f64) + 1.0);
    assert!(sender.set_parameters(invalid).is_err());
    let mut invalid = sender.parameters().unwrap();
    invalid.encodings[0].scale_resolution_down_to = Some(VideoResolution {
        width: i32::MAX as u32,
        height: 2,
    });
    assert!(sender.set_parameters(invalid).is_err());
    let mut current = sender.parameters().unwrap();
    current.encodings[0].active = true;
    current.encodings[0].scale_resolution_down_by = None;
    current.encodings[0].scale_resolution_down_to = None;
    current.encodings[0].max_framerate = None;
    sender.set_parameters(current).unwrap();
    assert!(sender.parameters().unwrap().encodings[0].active);
}

#[test]
fn media_removal_failure_and_repeated_teardown_are_safe() {
    for fail_encoder in [false, true] {
        let counters = Arc::new(Counters::default());
        let mut pair = Pair::new(counters.clone(), fail_encoder);
        let mut source = pair.alice_factory.create_video_source().unwrap();
        let track = pair
            .alice_factory
            .create_video_track("teardown", &source)
            .unwrap();
        let transceiver = pair
            .alice
            .add_video_transceiver(&track, RtpTransceiverDirection::SendReceive)
            .unwrap();
        transceiver
            .set_direction(RtpTransceiverDirection::SendOnly)
            .unwrap();
        assert_eq!(transceiver.direction(), RtpTransceiverDirection::SendOnly);
        let sender = transceiver.sender();
        let mut events = pair.negotiate();
        let remote = (0..2_000_000)
            .find_map(|_| {
                if let Some(remote) = take_remote_transceiver(&mut events) {
                    return Some(remote);
                }
                events.extend(pair.progress().1);
                None
            })
            .expect("remote teardown transceiver did not arrive");
        let mut sink = remote.receiver().track().unwrap().attach_sink().unwrap();
        source
            .push_frame(&VideoFrame::i420(4, 4, synthetic_i420(4, 4), 3_000_000, 180_000).unwrap())
            .unwrap();
        for _ in 0..50_000 {
            pair.progress();
            if counters.encoder_create.load(Ordering::SeqCst) > 0 {
                break;
            }
        }
        assert!(counters.encoder_create.load(Ordering::SeqCst) > 0);
        if fail_encoder {
            assert_eq!(counters.encode.load(Ordering::SeqCst), 0);
            assert!(sink.try_next_frame().is_none());
        }
        pair.alice.remove_track(&sender).unwrap();
        sink.close().unwrap();
        source.close().unwrap();
        assert_eq!(
            source.push_frame(&VideoFrame::i420(2, 2, vec![0; 6], 0, 0).unwrap()),
            Err(CodecError::Released)
        );
        pair.alice.close().unwrap();
        pair.bob.close().unwrap();
    }

    for index in 0..16 {
        let counters = Arc::new(Counters::default());
        let mut pair = Pair::new(counters, false);
        let mut source = pair.alice_factory.create_video_source().unwrap();
        let track = pair
            .alice_factory
            .create_video_track(&format!("repeat-{index}"), &source)
            .unwrap();
        let mut sink = track.attach_sink().unwrap();
        let frame = VideoFrame::i420(2, 2, vec![index as u8; 6], index as i64, index).unwrap();
        source.push_frame(&frame).unwrap();
        assert_eq!(sink.try_next_frame(), Some(frame));
        if index % 2 == 0 {
            sink.close().unwrap();
            source.close().unwrap();
            pair.alice.close().unwrap();
        } else {
            pair.alice.close().unwrap();
            source.close().unwrap();
            sink.close().unwrap();
        }
    }

    let counters = Arc::new(Counters::default());
    let mut pair = Pair::new(counters, false);
    let source = pair.alice_factory.create_video_source().unwrap();
    let track = pair
        .alice_factory
        .create_video_track("in-flight", &source)
        .unwrap();
    pair.alice
        .add_video_transceiver(&track, RtpTransceiverDirection::SendOnly)
        .unwrap();
    let _ = pair.negotiate();
    source
        .push_frame(&VideoFrame::i420(4, 4, synthetic_i420(4, 4), 0, 0).unwrap())
        .unwrap();
    pair.alice.close().unwrap();
}
