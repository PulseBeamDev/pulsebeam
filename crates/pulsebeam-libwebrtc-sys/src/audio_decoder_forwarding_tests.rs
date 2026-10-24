//! Real SDK-derived decoder forwarding, not factory/peer installation evidence.
use crate::{
    AudioDecoderFactory, Environment, RandomnessLease, RandomnessLeaseError, SdpAudioFormat, ffi,
};

fn forwarded(
    environment: &Environment,
    format: &SdpAudioFormat,
) -> cxx::UniquePtr<ffi::NativeAudioDecoder> {
    let source = AudioDecoderFactory::builtin()
        .unwrap()
        .create(environment, format)
        .unwrap()
        .unwrap();
    let transfer = match source.try_into_factory_transfer() {
        Ok(value) => value,
        Err(_) => panic!("fresh native product must permit consuming preparation"),
    };
    let value = ffi::audio_decoder_forward_product(Box::new(transfer));
    assert!(!value.is_null());
    value
}

#[test]
fn actual_sdk_decoder_forwarding_preserves_queries_frames_and_native_frame_retention() {
    let environment = Environment::builder().build().unwrap();
    for (name, rate, channels, payload) in [
        (b"PCMU".as_slice(), 8000, 1, vec![0xff; 160]),
        (b"PCMA".as_slice(), 8000, 2, vec![0xd5; 320]),
        (b"L16".as_slice(), 48000, 1, vec![0; 320]),
        (b"G722".as_slice(), 8000, 1, vec![0xff; 160]),
    ] {
        let format = SdpAudioFormat::new(name.to_vec(), rate, channels);
        let baseline = AudioDecoderFactory::builtin()
            .unwrap()
            .create(&environment, &format)
            .unwrap()
            .unwrap();
        let expected = baseline.packet_info(&payload).unwrap();
        let product = forwarded(&environment, &format);
        let native = product.as_ref().unwrap();
        assert_eq!(
            ffi::audio_decoder_sample_rate(native),
            baseline.sample_rate_hz()
        );
        assert_eq!(ffi::audio_decoder_channels(native), baseline.channels());
        let observed = ffi::audio_decoder_packet_info(native, &payload);
        assert!(observed.valid);
        assert_eq!(observed.duration, expected.duration);
        assert_eq!(observed.redundant_duration, expected.redundant_duration);
        assert_eq!(observed.has_fec, expected.has_fec);
        assert_eq!(ffi::audio_decoder_forwarded_frame_count(native), 0);
        let parsed = ffi::audio_decoder_parse(native, &payload, u32::MAX);
        assert!(parsed.valid);
        assert!(!parsed.frames.is_empty());
        assert!(ffi::audio_decoder_forwarded_frame_count(native) > 0);
        assert!(!ffi::audio_decoder_factory_transfer_ready(native));
        let reference = baseline.parse_payload(payload, u32::MAX).unwrap();
        assert_eq!(parsed.frames.len(), reference.len());
        for (actual, expected) in parsed.frames.iter().zip(reference) {
            assert_eq!(actual.timestamp, expected.timestamp);
            assert_eq!(actual.priority, expected.priority);
            let frame = actual.frame.as_ref().unwrap();
            let expected = expected.frame.unwrap();
            assert_eq!(ffi::audio_frame_duration(frame), expected.duration());
            assert_eq!(ffi::audio_frame_is_dtx(frame), expected.is_dtx_packet());
            let decoded = ffi::audio_frame_decode(frame);
            let reference = expected.decode().unwrap().unwrap();
            assert_eq!(decoded.status, 1);
            assert_eq!(decoded.samples, reference.samples);
            assert_eq!(decoded.speech_type, reference.speech_type.0);
            assert_eq!(ffi::audio_frame_decode(frame).status, 2);
        }
        drop(parsed);
        assert_eq!(ffi::audio_decoder_forwarded_frame_count(native), 0);
        assert!(ffi::audio_decoder_factory_transfer_ready(native));
        ffi::audio_decoder_reset(native);
        baseline.reset();
        assert_eq!(ffi::audio_decoder_error_code(native), baseline.error_code());
        let plc = ffi::audio_decoder_generate_plc(native, 1, &[-10, 11]);
        assert!(plc.valid);
        let mut reference_plc = vec![-10, 11];
        baseline.generate_plc(1, &mut reference_plc).unwrap();
        assert_eq!(plc.samples, reference_plc);
    }
}

#[test]
fn nested_forwarding_retains_actual_original_decoder_after_all_outer_owners_drop() {
    let environment = Environment::builder().build().unwrap();
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    let once = forwarded(&environment, &format);
    let source = crate::AudioDecoder::from_native(once, environment.clone());
    let transfer = match source.try_into_factory_transfer() {
        Ok(value) => value,
        Err(_) => panic!("fresh forwarded product should permit another handoff"),
    };
    let twice = ffi::audio_decoder_forward_product(Box::new(transfer));
    assert!(!twice.is_null());
    let parsed = ffi::audio_decoder_parse(twice.as_ref().unwrap(), &[0xff; 160], 123);
    assert!(parsed.valid);
    // Each real forwarding layer owns the underlying frame and shares one
    // private lifetime counter, without changing duration/timestamp/priority.
    assert_eq!(
        ffi::audio_decoder_forwarded_frame_count(twice.as_ref().unwrap()),
        2
    );
    drop(twice);
    drop(environment);
    let frame = parsed.frames[0].frame.as_ref().unwrap();
    assert_eq!(ffi::audio_frame_duration(frame), 160);
    let decoded = ffi::audio_frame_decode(frame);
    assert_eq!(decoded.status, 1);
    assert_eq!(decoded.samples, vec![0; 160]);
}

#[test]
#[ignore = "exclusive process-global randomness; run this fixture alone with --ignored --test-threads=1"]
fn forwarded_frames_retain_rust_randomness_lease_after_native_product_retirement() {
    let lease = RandomnessLease::acquire(123).unwrap();
    let environment = Environment::builder().randomness(&lease).build().unwrap();
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    let product = forwarded(&environment, &format);
    let parsed = ffi::audio_decoder_parse(product.as_ref().unwrap(), &[0xff; 160], 0);
    assert!(parsed.valid);
    assert!(!parsed.frames.is_empty());
    drop(product);
    drop(environment);
    drop(lease);
    assert_eq!(
        RandomnessLease::acquire(456).err(),
        Some(RandomnessLeaseError::AlreadyAcquired)
    );
    let frame = parsed.frames[0].frame.as_ref().unwrap();
    assert_eq!(ffi::audio_frame_decode(frame).status, 1);
    assert_eq!(
        RandomnessLease::acquire(456).err(),
        Some(RandomnessLeaseError::AlreadyAcquired)
    );
    drop(parsed);
    let replacement = RandomnessLease::acquire(456).unwrap();
    drop(replacement);
}
