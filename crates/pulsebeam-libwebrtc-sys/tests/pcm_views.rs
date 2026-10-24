use pulsebeam_webrtc_sys::*;

#[test]
fn native_shapes_and_flat_borrows_keep_their_actual_domain() {
    let data = [1i16, 11, 2, 22, 99];
    let view = InterleavedPcm::new(&data, 2, 2).unwrap();
    assert_eq!(
        (view.num_channels(), view.samples_per_channel(), view.size()),
        (2, 2, 4)
    );
    assert!(!view.is_empty() && !view.is_mono() && view.is_interleaved());
    assert_eq!(&*view, &[1, 11, 2, 22]);
    assert_eq!(view.iter().copied().collect::<Vec<_>>(), [1, 11, 2, 22]);
    assert_eq!(
        view.iter().rev().copied().collect::<Vec<_>>(),
        [22, 2, 11, 1]
    );
    assert_eq!(view.as_mono().err(), Some(PcmViewError::NotMono));
    assert_eq!(
        InterleavedPcm::from_samples(&data, 2).err(),
        Some(PcmViewError::InvalidShape)
    );
    assert!(InterleavedPcm::new(&data[..2], 1, 2).is_ok()); // Native view, unlike AudioFrame mut-shape.
    assert_eq!(
        InterleavedPcm::new(&data, 0, 1).err(),
        Some(PcmViewError::InvalidShape)
    );
    assert_eq!(
        InterleavedPcm::new(&data, 1, 17).err(),
        Some(PcmViewError::InvalidShape)
    );
    assert_eq!(
        InterleavedPcm::new(&data, usize::MAX, 2).err(),
        Some(PcmViewError::InvalidShape)
    );
    let empty = InterleavedPcm::<i16>::new(&[], usize::MAX, 0).unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.samples_per_channel(), usize::MAX);
    assert_eq!(empty.num_channels(), 0);
    assert!(MonoPcm::<i16>::new(&[]).is_mono());
    let mono = InterleavedPcm::from_samples(&data, 1).unwrap();
    assert_eq!(&*mono.as_mono().unwrap(), &data);
}

#[test]
fn copy_from_and_free_copy_samples_use_native_operations_and_preflight() {
    let input = [1i16, 2, 3, 4];
    let source = InterleavedPcm::new(&input, 2, 2).unwrap();
    let mut output = [9; 6];
    {
        let mut destination = InterleavedPcmMut::new(&mut output, 2, 2).unwrap();
        destination.copy_from(&source).unwrap();
        assert_eq!(&*destination, &input);
        destination[0] = 17;
        assert_eq!(destination.as_const()[0], 17);
        copy_samples(&mut destination, &source).unwrap();
        let wrong = InterleavedPcm::new(&input, 4, 1).unwrap();
        assert_eq!(
            destination.copy_from(&wrong),
            Err(PcmViewError::ShapeMismatch)
        );
    }
    assert_eq!(output, [1, 2, 3, 4, 9, 9]);
    for (dm, sm) in [(true, true), (true, false), (false, true)] {
        let mut bytes = [9; 4];
        if dm {
            let mut destination = MonoPcmMut::new(&mut bytes);
            if sm {
                copy_samples(&mut destination, &MonoPcm::new(&input)).unwrap();
            } else {
                copy_samples(
                    &mut destination,
                    &InterleavedPcm::new(&input, 4, 1).unwrap(),
                )
                .unwrap();
            }
        } else {
            let mut destination = InterleavedPcmMut::new(&mut bytes, 4, 1).unwrap();
            copy_samples(&mut destination, &MonoPcm::new(&input)).unwrap();
        }
        assert_eq!(bytes, input);
    }
    assert_eq!(
        copy_samples(&mut MonoPcmMut::<i16>::new(&mut []), &MonoPcm::new(&[])),
        Err(PcmViewError::EmptyCopy)
    );
}

#[test]
fn native_float_copy_preserves_bits_including_nan_and_negative_zero() {
    let bits = [0x7fc12345, 0x80000000, 0x7f800000, 0xff800000];
    let input = bits.map(f32::from_bits);
    let mut output = [17.0; 6];
    {
        let source = InterleavedPcm::new(&input, 2, 2).unwrap();
        let mut destination = InterleavedPcmMut::new(&mut output, 2, 2).unwrap();
        destination.copy_from(&source).unwrap();
        assert_eq!(
            destination.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            bits
        );
        copy_samples(&mut destination, &source).unwrap();
    }
    assert_eq!(
        output[..4].iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        bits
    );
    assert_eq!(&output[4..], &[17.0; 2]);
}

#[test]
fn planar_storage_channels_and_native_clear_preserve_prefixes_and_representation() {
    let input = [1i16, 2, 3, 11, 22, 33, 99];
    let planar = DeinterleavedPcm::coallocated(&input, 3, 2).unwrap();
    assert!(!planar.is_interleaved() && !planar.is_mono());
    assert_eq!(&*planar.channel(1).unwrap(), &[11, 22, 33]);
    assert_eq!(&*planar.as_mono().unwrap(), &[1, 2, 3]); // Native returns first channel, not an IsMono check.
    assert!(planar.channel(2).is_none());
    let mut a = [1i16, 2, 3, 7];
    let mut b = [4i16, 5, 6, 8];
    {
        let mut view = DeinterleavedPcmMut::channels(vec![&mut a, &mut b], 3).unwrap();
        assert_eq!(view.size(), 6);
        assert_eq!(&*view.as_const().channel(1).unwrap(), &[4, 5, 6]);
        view.channel_mut(0).unwrap()[1] = 19;
        assert_eq!(view.as_mono_mut().unwrap()[1], 19);
        view.clear();
    }
    assert_eq!(a, [0, 0, 0, 7]);
    assert_eq!(b, [0, 0, 0, 8]);
    assert!(DeinterleavedPcm::channels(vec![&a[..1], &b], 2).is_err());
    let mut many = [5i16; 18];
    {
        // The planar constructor has no interleaved 16-channel restriction.
        let mut view = DeinterleavedPcmMut::coallocated(&mut many, 1, 17).unwrap();
        assert_eq!(view.num_channels(), 17);
        view.clear();
    }
    assert_eq!(&many[..17], &[0; 17]);
    assert_eq!(many[17], 5);
    let mut floats = [f32::NAN, -0.0, 1.0, 2.0, 9.0];
    DeinterleavedPcmMut::coallocated(&mut floats, 2, 2)
        .unwrap()
        .clear();
    assert!(floats[..4].iter().all(|v| v.to_bits() == 0));
    assert_eq!(floats[4], 9.0);
    let mut empty = DeinterleavedPcmMut::<i16>::channels(vec![], 99).unwrap();
    assert!(empty.is_empty());
    empty.clear();
}

#[test]
fn unavailable_native_planar_copy_is_not_replaced_with_a_copy_loop() {
    let input = [1i16, 2, 3, 4];
    let source = DeinterleavedPcm::coallocated(&input, 2, 2).unwrap();
    let mut output = [17; 4];
    let mut destination = DeinterleavedPcmMut::coallocated(&mut output, 2, 2).unwrap();
    assert_eq!(
        copy_samples(&mut destination, &source),
        Err(PcmViewError::NativePlanarCopyUnavailable)
    );
    assert_eq!(&*destination.channel(0).unwrap(), &[17; 2]);
    assert_eq!(&*destination.channel(1).unwrap(), &[17; 2]);
}

#[test]
fn typed_frame_loans_preserve_native_mute_prefix_and_metadata_semantics() {
    let mut frame = AudioFrame::new();
    frame.mutable_data().fill(7);
    frame.set_absolute_capture_timestamp_ms(-13);
    frame
        .set_fields(&AudioFrameFields {
            samples_per_channel: 4,
            num_channels: 2,
            ..Default::default()
        })
        .unwrap();
    frame.mute();
    assert_eq!(&*frame.interleaved_view().unwrap(), &[0; 8]);
    {
        let mut view = frame.interleaved_view_mut_with_shape(2, 2).unwrap();
        assert_eq!((view.samples_per_channel(), view.num_channels()), (2, 2));
        assert_eq!(&*view, &[0; 4]);
        view[0] = 19;
    }
    assert_eq!(&frame.data()[..8], &[19, 0, 0, 0, 7, 7, 7, 7]);
    assert_eq!(frame.absolute_capture_timestamp_ms(), Some(-13));
    let before = frame.fields();
    assert_eq!(
        frame.interleaved_view_mut_with_shape(1, 2).err(),
        Some(AudioFrameValueError::InvalidShape)
    );
    assert_eq!(frame.fields(), before);
    {
        let mut callback = frame.as_mut();
        let mut view = callback.interleaved_view_mut_with_shape(2, 2).unwrap();
        view[3] = 29;
    }
    assert_eq!(&*frame.interleaved_view().unwrap(), &[19, 0, 0, 29]);
}
