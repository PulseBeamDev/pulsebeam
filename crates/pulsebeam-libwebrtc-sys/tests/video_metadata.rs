use pulsebeam_webrtc_sys::FrameInstrumentationData;

#[test]
fn native_defaults_and_convenience_methods() {
    let mut value = FrameInstrumentationData::new();
    assert_eq!(value.sequence_index(), 0);
    assert!(!value.is_droppable());
    assert_eq!(value.std_dev(), 0.0);
    assert_eq!(value.luma_error_threshold(), 0);
    assert_eq!(value.chroma_error_threshold(), 0);
    assert!(value.sample_values().is_empty());
    assert!(value.is_sync_only());
    assert!(value.holds_upper_bits());

    for (index, upper) in [
        (1, false),
        (127, false),
        (128, true),
        (256, true),
        (16383, false),
    ] {
        assert!(value.set_sequence_index(index));
        assert_eq!(value.holds_upper_bits(), upper);
        value.set_droppable(true);
        assert!(!value.holds_upper_bits());
        value.set_droppable(false);
    }
    assert!(value.set_sample_values(&[0.0]));
    assert!(!value.is_sync_only());
    assert!(value.set_sample_values_owned(Vec::new()));
    assert!(value.is_sync_only());
}

#[test]
fn sdk_scalar_domains_and_rejection_preserve_previous_values() {
    let mut value = FrameInstrumentationData::default();
    for index in [0, 1, 128, 16383] {
        assert!(value.set_sequence_index(index));
        assert_eq!(value.sequence_index(), index);
    }
    for index in [i32::MIN, -1, 16384, i32::MAX] {
        assert!(!value.set_sequence_index(index));
        assert_eq!(value.sequence_index(), 16383);
    }
    for dev in [-0.0, 0.0, 0.25, 40.0] {
        assert!(value.set_std_dev(dev));
        assert_eq!(value.std_dev().to_bits(), dev.to_bits());
    }
    for dev in [f64::NEG_INFINITY, -0.01, 40.01, f64::INFINITY] {
        assert!(!value.set_std_dev(dev));
        assert_eq!(value.std_dev(), 40.0);
    }
    for threshold in 0..=15 {
        assert!(value.set_luma_error_threshold(threshold));
        assert!(value.set_chroma_error_threshold(threshold));
        assert_eq!(value.luma_error_threshold(), threshold);
        assert_eq!(value.chroma_error_threshold(), threshold);
    }
    for threshold in [i32::MIN, -1, 16, i32::MAX] {
        assert!(!value.set_luma_error_threshold(threshold));
        assert!(!value.set_chroma_error_threshold(threshold));
        assert_eq!(value.luma_error_threshold(), 15);
        assert_eq!(value.chroma_error_threshold(), 15);
    }
}

#[test]
fn sample_overloads_copy_or_consume_without_an_adapter_length_cap() {
    let mut value = FrameInstrumentationData::new();
    let mut source = vec![0.0, -0.0, 128.5, 255.0];
    assert!(value.set_sample_values(&source));
    source[0] = 12.0;
    let samples = value.sample_values();
    assert_eq!(samples, vec![0.0, -0.0, 128.5, 255.0]);
    assert_eq!(samples[1].to_bits(), (-0.0f64).to_bits());

    for invalid in [-0.01, 255.01, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(!value.set_sample_values(&[1.0, invalid, 2.0]));
        assert_eq!(value.sample_values(), samples);
        assert!(!value.set_sample_values_owned(vec![3.0, invalid, 4.0]));
        assert_eq!(value.sample_values(), samples);
    }

    let many = vec![3.5; 1024];
    assert!(value.set_sample_values_owned(many.clone()));
    assert_eq!(value.sample_values(), many);
    let mut detached = value.sample_values();
    detached[0] = 77.0;
    assert_eq!(value.sample_values()[0], 3.5);
    assert!(value.set_sample_values(&[]));
    assert!(value.is_sync_only());
}

#[test]
fn sdk_accepted_nan_metadata_is_not_silently_normalized_or_rejected() {
    let mut value = FrameInstrumentationData::new();
    let nan = f64::from_bits(0x7ff8_0000_0000_1234);
    assert!(value.set_std_dev(nan));
    assert_eq!(value.std_dev().to_bits(), nan.to_bits());
    assert!(value.set_sample_values(&[nan]));
    assert_eq!(value.sample_values()[0].to_bits(), nan.to_bits());
    assert!(value.set_sample_values_owned(vec![nan, 1.0]));
    let copy = value.clone();
    assert_eq!(copy.std_dev().to_bits(), nan.to_bits());
    assert_eq!(copy.sample_values()[0].to_bits(), nan.to_bits());
    let mut assigned = FrameInstrumentationData::new();
    assigned.assign_from(&copy);
    assert_eq!(assigned.std_dev().to_bits(), nan.to_bits());
    assert_eq!(assigned.sample_values()[0].to_bits(), nan.to_bits());
}

#[test]
fn sdk_copy_assignment_is_independent_and_values_have_no_origin_role() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<FrameInstrumentationData>();
    let mut source = FrameInstrumentationData::new();
    assert!(source.set_sequence_index(256));
    source.set_droppable(true);
    assert!(source.set_std_dev(12.5));
    assert!(source.set_luma_error_threshold(7));
    assert!(source.set_chroma_error_threshold(9));
    assert!(source.set_sample_values_owned(vec![4.0, 5.0]));

    let mut copied = source.clone();
    let mut assigned = FrameInstrumentationData::new();
    assigned.assign_from(&source);
    assert!(source.set_sequence_index(1));
    assert!(source.set_sample_values_owned(vec![99.0]));
    drop(source);
    assert_eq!(copied.sequence_index(), 256);
    assert!(copied.is_droppable());
    assert_eq!(copied.std_dev(), 12.5);
    assert_eq!(copied.luma_error_threshold(), 7);
    assert_eq!(copied.chroma_error_threshold(), 9);
    assert_eq!(copied.sample_values(), vec![4.0, 5.0]);
    assert!(copied.set_sample_values_owned(vec![8.0]));
    drop(copied);

    std::thread::spawn(move || {
        assert_eq!(assigned.sequence_index(), 256);
        assert_eq!(assigned.sample_values(), vec![4.0, 5.0]);
        assert!(assigned.set_sequence_index(128));
        assigned.set_droppable(false);
        assert!(assigned.holds_upper_bits());
        // Final SDK vector destruction is independent of the creating thread.
    })
    .join()
    .unwrap();
}
