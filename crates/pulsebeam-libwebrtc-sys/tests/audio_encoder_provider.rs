//! Caller product dispatch contracts with real delegated SDK PCM, not a fake
//! wire codec or proof of native caller-product installation.
use pulsebeam_webrtc_sys::{
    AudioEncoder, AudioEncoderError, AudioEncoderFactory, AudioEncoderFrameLengthRange,
    AudioEncoderOutput, AudioEncoderProductError, AudioEncoderProductFailureKind,
    AudioEncoderProductOperation, AudioEncoderProvider, BitrateAllocationUpdate,
    CallerAudioEncoder, DataRate, Environment, ManualClock, OwnedAudioEncoderOptions,
    SdpAudioFormat, TimeDelta,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Pcm {
    encoder: AudioEncoder,
    calls: Arc<AtomicUsize>,
    bandwidth: Arc<Mutex<Vec<(i32, Option<i64>)>>>,
    contexts: Environment,
    corrupt: bool,
    panic_encode: bool,
    panic_drop: bool,
    wrapped_shape: bool,
}
impl Drop for Pcm {
    fn drop(&mut self) {
        if self.panic_drop {
            panic!("caller encoder destructor");
        }
    }
}
impl AudioEncoderProvider for Pcm {
    fn sample_rate_hz(&mut self) -> i32 {
        if self.wrapped_shape {
            100
        } else {
            self.encoder.sample_rate_hz()
        }
    }
    fn channels(&mut self) -> usize {
        if self.wrapped_shape {
            1usize << (usize::BITS - 1)
        } else {
            self.encoder.channels()
        }
    }
    fn frames_in_next_packet(&mut self) -> usize {
        self.encoder.frames_in_next_packet()
    }
    fn maximum_frames_in_packet(&mut self) -> usize {
        self.encoder.maximum_frames_in_packet()
    }
    fn target_bitrate_bps(&mut self) -> i32 {
        self.encoder.target_bitrate_bps()
    }
    fn encode(
        &mut self,
        timestamp: u32,
        audio: &[i16],
    ) -> Result<AudioEncoderOutput, AudioEncoderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.panic_encode {
            panic!("caller Encode callback");
        }
        assert_eq!(self.contexts.timestamp().microseconds().unwrap(), 17000);
        let mut output = self.encoder.encode(timestamp, audio)?;
        if self.corrupt {
            self.corrupt = false;
            output.info.primary.encoded_bytes += 1;
        }
        Ok(output)
    }
    fn reset(&mut self) -> Result<(), AudioEncoderError> {
        self.encoder.reset();
        Ok(())
    }
    fn frame_length_range(&mut self) -> Option<AudioEncoderFrameLengthRange> {
        self.encoder.frame_length_range()
    }
    fn on_received_uplink_bandwidth(
        &mut self,
        bitrate: i32,
        period: Option<i64>,
    ) -> Result<(), AudioEncoderError> {
        self.bandwidth.lock().unwrap().push((bitrate, period));
        self.encoder.on_received_uplink_bandwidth(bitrate, period)
    }
}
fn setup(
    corrupt: bool,
    panic_encode: bool,
    panic_drop: bool,
    wrapped_shape: bool,
) -> (
    CallerAudioEncoder,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<(i32, Option<i64>)>>>,
) {
    let clock = ManualClock::new(Duration::from_millis(17)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let factory = AudioEncoderFactory::builtin().unwrap();
    let mut options = OwnedAudioEncoderOptions::default();
    options.set_payload_type(200);
    let encoder = factory
        .create_with_options(
            &environment,
            &SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1),
            &options,
        )
        .unwrap()
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let bandwidth = Arc::new(Mutex::new(Vec::new()));
    let product = CallerAudioEncoder::from_provider(
        &environment,
        Pcm {
            encoder,
            calls: calls.clone(),
            bandwidth: bandwidth.clone(),
            contexts: environment.clone(),
            corrupt,
            panic_encode,
            panic_drop,
            wrapped_shape,
        },
    );
    drop(factory);
    drop(options);
    drop(environment);
    drop(clock);
    (product, calls, bandwidth)
}
#[test]
fn owning_caller_product_delegates_real_pcm_and_survives_migrated_use() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<CallerAudioEncoder>();
    let (product, calls, _) = setup(false, false, false, false);
    let alias = product.clone();
    assert_eq!(product.sample_rate_hz(), Ok(8000));
    assert_eq!(product.channels(), Ok(1));
    assert_eq!(product.rtp_timestamp_rate_hz(), Ok(8000));
    assert_eq!(product.frames_in_next_packet(), Ok(2));
    assert_eq!(product.maximum_frames_in_packet(), Ok(2));
    assert_eq!(product.bitrate_range(), Ok(None));
    assert_eq!(
        product.encode(100, &[0; 79]).err(),
        Some(AudioEncoderProductError {
            operation: AudioEncoderProductOperation::Encode,
            kind: AudioEncoderProductFailureKind::InvalidSamples,
        })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(product.encode(100, &[0; 80]).unwrap().bytes.is_empty());
    drop(product);
    std::thread::spawn(move || {
        let output = alias.encode(180, &[0; 80]).unwrap();
        assert_eq!(output.bytes.len(), 160);
        assert_eq!(output.info.primary.encoded_bytes, 160);
        assert_eq!(output.info.primary.encoded_timestamp, 100);
        assert_eq!(output.info.primary.payload_type, 200);
        assert!(output.info.redundant.is_empty());
        assert!(!alias.failure_observer().is_disabled());
    })
    .join()
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
#[test]
fn default_controls_keep_native_absence_and_bandwidth_delegation() {
    let (product, _, bandwidth) = setup(false, false, false, false);
    assert_eq!(product.set_fec(false), Ok(true));
    assert_eq!(product.set_fec(true), Ok(false));
    assert_eq!(product.set_dtx(false), Ok(true));
    assert_eq!(product.set_dtx(true), Ok(false));
    assert_eq!(product.dtx(), Ok(false));
    assert_eq!(product.enable_audio_network_adaptor(b"\xff\0"), Ok(false));
    assert!(
        product
            .network_adaptation_statistics()
            .unwrap()
            .bitrate_action_counter
            .is_none()
    );
    product.on_received_target_audio_bitrate(-1).unwrap();
    product
        .on_received_uplink_allocation(BitrateAllocationUpdate {
            target_bitrate: DataRate::BitsPerSecond(i64::MAX - 1),
            bwe_period: TimeDelta::from_microseconds(-1500).unwrap(),
            ..BitrateAllocationUpdate::default()
        })
        .unwrap();
    assert_eq!(
        *bandwidth.lock().unwrap(),
        vec![
            (-1, None),
            (
                -2,
                Some(
                    TimeDelta::from_microseconds(-1500)
                        .unwrap()
                        .millis()
                        .unwrap()
                )
            )
        ]
    );
    match product.on_received_uplink_allocation(BitrateAllocationUpdate::default()) {
        Err(error) => assert_eq!(
            error,
            AudioEncoderProductError {
                operation: AudioEncoderProductOperation::UplinkAllocation,
                kind: AudioEncoderProductFailureKind::Provider(AudioEncoderError::InvalidControl),
            }
        ),
        Ok(()) => assert_eq!(
            bandwidth.lock().unwrap().last().copied(),
            Some((0, Some(9_223_372_036_854_776)))
        ),
    }
    match product.on_received_uplink_allocation(BitrateAllocationUpdate {
        target_bitrate: DataRate::PositiveInfinity,
        bwe_period: TimeDelta::minus_infinity(),
        ..BitrateAllocationUpdate::default()
    }) {
        Err(error) => assert_eq!(
            error.kind,
            AudioEncoderProductFailureKind::Provider(AudioEncoderError::InvalidControl)
        ),
        Ok(()) => assert_eq!(
            bandwidth.lock().unwrap().last().copied(),
            Some((-1, Some(-9_223_372_036_854_776)))
        ),
    }
    assert_eq!(
        product
            .on_received_uplink_recoverable_packet_loss_fraction(0.0)
            .err(),
        Some(AudioEncoderProductError {
            operation: AudioEncoderProductOperation::RecoverablePacketLoss,
            kind: AudioEncoderProductFailureKind::Provider(AudioEncoderError::UnsupportedControl),
        })
    );
}
#[test]
fn malformed_owned_output_is_rejected_not_repaired_or_replaced_by_buffering() {
    let (product, calls, _) = setup(true, false, false, false);
    assert_eq!(
        product.encode(100, &[0; 80]).err(),
        Some(AudioEncoderProductError {
            operation: AudioEncoderProductOperation::Encode,
            kind: AudioEncoderProductFailureKind::InvalidOutput,
        })
    );
    assert!(!product.failure_observer().is_disabled());
    // State is neither reset nor synthesized: the real backend accumulated the
    // first block, and the next correctly reported output contains that packet.
    let next = product.encode(180, &[0; 80]).unwrap();
    assert_eq!(next.bytes.len(), 160);
    assert_eq!(next.info.primary.encoded_timestamp, 100);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
#[test]
fn callback_panic_is_typed_and_disables_further_provider_entry() {
    let (product, calls, _) = setup(false, true, false, false);
    let observer = product.failure_observer();
    let error = AudioEncoderProductError {
        operation: AudioEncoderProductOperation::Encode,
        kind: AudioEncoderProductFailureKind::Panicked,
    };
    assert_eq!(product.encode(0, &[0; 80]), Err(error));
    assert_eq!(
        product.sample_rate_hz(),
        Err(AudioEncoderProductError {
            operation: AudioEncoderProductOperation::SampleRate,
            kind: AudioEncoderProductFailureKind::Disabled,
        })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(product);
    assert_eq!(observer.failure(), Some(error));
    assert!(observer.is_disabled());
}
#[test]
fn destructor_failure_observer_outlives_last_owner() {
    let (product, _, _) = setup(false, false, true, false);
    let observer = product.failure_observer();
    let alias = product.clone();
    drop(product);
    assert_eq!(observer.failure(), None);
    drop(alias);
    assert_eq!(
        observer.failure(),
        Some(AudioEncoderProductError {
            operation: AudioEncoderProductOperation::Destroy,
            kind: AudioEncoderProductFailureKind::Panicked,
        })
    );
}
#[test]
fn standalone_shape_uses_native_unsigned_wrap_not_a_metadata_whitelist() {
    let (product, calls, _) = setup(false, false, false, true);
    assert_eq!(product.channels(), Ok(1usize << (usize::BITS - 1)));
    // Native size_t channels*100 wraps to zero. This is observation/standalone
    // dispatch, not authorization to feed that metadata to SDK media consumers.
    assert_eq!(
        product.encode(0, &[]).err(),
        Some(AudioEncoderProductError {
            operation: AudioEncoderProductOperation::Encode,
            kind: AudioEncoderProductFailureKind::Provider(AudioEncoderError::InvalidSamples),
        })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn mutable_caller_callbacks_serialize_concurrent_real_codec_operations() {
    let (product, calls, _) = setup(false, false, false, false);
    let mut threads = Vec::new();
    for lane in 0..4 {
        let product = product.clone();
        threads.push(std::thread::spawn(move || {
            (0..25)
                .map(|index| {
                    product
                        .encode(lane * 1000 + index * 80, &[0; 80])
                        .unwrap()
                        .bytes
                        .len()
                })
                .sum::<usize>()
        }));
    }
    assert_eq!(
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .sum::<usize>(),
        50 * 160
    );
    assert_eq!(calls.load(Ordering::SeqCst), 100);
    assert_eq!(product.failure_observer().failure(), None);
}

struct Reentrant {
    slot: Arc<Mutex<Option<CallerAudioEncoder>>>,
}
impl AudioEncoderProvider for Reentrant {
    fn sample_rate_hz(&mut self) -> i32 {
        8000
    }
    fn channels(&mut self) -> usize {
        1
    }
    fn frames_in_next_packet(&mut self) -> usize {
        2
    }
    fn maximum_frames_in_packet(&mut self) -> usize {
        2
    }
    fn target_bitrate_bps(&mut self) -> i32 {
        assert_eq!(
            self.slot.lock().unwrap().as_ref().unwrap().sample_rate_hz(),
            Err(AudioEncoderProductError {
                operation: AudioEncoderProductOperation::SampleRate,
                kind: AudioEncoderProductFailureKind::ReentrantCall
            })
        );
        -1
    }
    fn encode(&mut self, _: u32, _: &[i16]) -> Result<AudioEncoderOutput, AudioEncoderError> {
        Err(AudioEncoderError::UnsupportedControl)
    }
    fn reset(&mut self) -> Result<(), AudioEncoderError> {
        Ok(())
    }
    fn frame_length_range(&mut self) -> Option<AudioEncoderFrameLengthRange> {
        None
    }
}
#[test]
fn same_product_reentry_is_rejected_before_mutex_and_does_not_disable_future_calls() {
    let environment = Environment::builder().build().unwrap();
    let slot = Arc::new(Mutex::new(None));
    let product = CallerAudioEncoder::from_provider(&environment, Reentrant { slot: slot.clone() });
    *slot.lock().unwrap() = Some(product.clone());
    assert_eq!(product.target_bitrate_bps(), Ok(-1));
    assert_eq!(product.sample_rate_hz(), Ok(8000));
    assert!(!product.failure_observer().is_disabled());
    assert_eq!(
        product.failure_observer().failure().unwrap().kind,
        AudioEncoderProductFailureKind::ReentrantCall
    );
    slot.lock().unwrap().take(); // Break the application's strong-reference cycle.
}
