//! Internal one-shot ownership preparation, not a completed SDK factory adapter.
use crate::{
    AudioDecoder, AudioDecoderFactory, AudioEncoderFactory, Environment, ManualClock,
    OwnedAudioEncoderOptions, SdpAudioFormat, ffi,
};
use std::time::Duration;

#[test]
fn encoder_handoff_refuses_aliases_and_keeps_actual_product_and_environment() {
    let clock = ManualClock::new(Duration::from_millis(7)).unwrap();
    let environment = Environment::builder().clock(&clock).build().unwrap();
    let factory = AudioEncoderFactory::builtin().unwrap();
    let mut options = OwnedAudioEncoderOptions::default();
    options.set_payload_type(200);
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    let encoder = factory
        .create_with_options(&environment, &format, &options)
        .unwrap()
        .unwrap();
    let alias = encoder.clone();
    let encoder = match encoder.try_into_factory_transfer() {
        Err(original) => original,
        Ok(_) => panic!("shared encoder must not be transferred"),
    };
    assert_eq!(encoder.sample_rate_hz(), alias.sample_rate_hz());
    // Real packet accumulation survives failed ownership transfer, no new codec.
    assert!(alias.encode(100, &[0; 80]).unwrap().bytes.is_empty());
    drop(alias);
    let transfer = match encoder.try_into_factory_transfer() {
        Ok(transfer) => transfer,
        Err(_) => panic!("unique encoder transfer should be prepared"),
    };
    drop(factory);
    drop(options);
    drop(environment);
    drop(clock);
    assert_eq!(
        transfer._environment.timestamp().microseconds().unwrap(),
        7000
    );
    let native = transfer.native.as_ref().unwrap();
    assert_eq!(ffi::audio_encoder_info(native).sample_rate_hz, 8000);
    let output = ffi::audio_encoder_encode(native, 180, &[0; 80]);
    assert_eq!(output.status, 0);
    assert_eq!(output.primary.encoded_timestamp, 100);
    assert_eq!(output.primary.payload_type, 200);
    assert_eq!(output.bytes.len(), 160);
}

#[test]
fn decoder_handoff_refuses_rust_and_native_frame_owners_without_detaching_backend() {
    let environment = Environment::builder().build().unwrap();
    let factory = AudioDecoderFactory::builtin().unwrap();
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    let decoder = factory.create(&environment, &format).unwrap().unwrap();
    let frames = decoder.parse_payload(vec![0xff; 160], 0).unwrap();
    assert!(!frames.is_empty());
    let decoder = match decoder.try_into_factory_transfer() {
        Err(original) => original,
        Ok(_) => panic!("Rust parsed-frame owners must prevent transfer"),
    };
    assert_eq!(decoder.sample_rate_hz(), 8000);
    drop(frames);
    let transfer = match decoder.try_into_factory_transfer() {
        Ok(transfer) => transfer,
        Err(_) => panic!("unique decoder transfer should be prepared"),
    };
    // Exercise actual native frame ownership without adding a Rust decoder Arc.
    // This is stock ParsePayload, not a manufactured frame or hidden registry.
    let frames = ffi::audio_decoder_parse(transfer.native.as_ref().unwrap(), &[0xff; 160], 0);
    assert!(frames.valid);
    assert!(!frames.frames.is_empty());
    assert!(!ffi::audio_decoder_factory_transfer_ready(
        transfer.native.as_ref().unwrap()
    ));
    let decoder = AudioDecoder::from_native(transfer.native, transfer._environment);
    let decoder = match decoder.try_into_factory_transfer() {
        Err(original) => original,
        Ok(_) => panic!("native shared-State frame owners must prevent transfer"),
    };
    assert_eq!(decoder.channels(), 1);
    drop(frames);
    let transfer = match decoder.try_into_factory_transfer() {
        Ok(transfer) => transfer,
        Err(_) => panic!("retired frame owners should allow handoff preparation"),
    };
    drop(factory);
    drop(environment);
    assert_eq!(
        ffi::audio_decoder_sample_rate(transfer.native.as_ref().unwrap()),
        8000
    );
    assert_eq!(
        ffi::audio_decoder_channels(transfer.native.as_ref().unwrap()),
        1
    );
    assert!(ffi::audio_decoder_factory_transfer_ready(
        transfer.native.as_ref().unwrap()
    ));
}
