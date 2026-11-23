#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "bounded wire-format fixtures"
)]

use super::wire::*;
use pulsebeam_feedback::{PacketId, ReceiverTime};

#[test]
fn data_framing_preserves_opaque_payload_and_rejects_truncated_headers() {
    let mut bytes = [0u8; DATA_HEADER + 3];
    data_header(&mut bytes, [1, 2, 3, 4], u32::MAX).unwrap();
    bytes[DATA_HEADER..].copy_from_slice(&[9, 8, 7]);
    assert_eq!(bytes, [1, 0, 1, 2, 3, 4, 255, 255, 255, 255, 9, 8, 7]);
    for length in 0..DATA_HEADER {
        assert!(decode(&bytes[..length]).is_err());
    }
    assert!(matches!(
        decode(&bytes),
        Ok(Frame::Data {
            context: Some([1, 2, 3, 4]),
            attempt: u32::MAX,
            payload: [9, 8, 7]
        })
    ));
    bytes[0] = 2;
    assert!(decode(&bytes).is_err());
    bytes[0] = 1;
    bytes[1] = 255;
    assert!(decode(&bytes).is_err());
    let mut oversize = vec![0u8; DATA_HEADER + PAYLOAD_BUDGET + 1];
    data_header(&mut oversize, [0, 0, 0, 1], 1).unwrap();
    assert!(decode(&oversize).is_err());
}

#[test]
fn fabric_attempt_frame_carries_one_unchanged_image_without_legacy_context() {
    let image = [0x90, 0xe0, 0x01, 0x02, 0x03];
    let mut bytes = [0; FABRIC_HEADER + 5];
    fabric_header(&mut bytes, 0x0123_4567, false).unwrap();
    bytes[FABRIC_HEADER..].copy_from_slice(&image);
    assert_eq!(&bytes[..FABRIC_HEADER], &[2, 0, 1, 0x23, 0x45, 0x67]);
    assert!(
        matches!(decode_for(&bytes, true), Ok(Frame::Data { context: None, attempt: 0x0123_4567, payload }) if payload == image)
    );
    assert!(decode(&bytes).is_err());
    for length in 0..=FABRIC_HEADER {
        assert!(decode_for(&bytes[..length], true).is_err());
    }
    let mut legacy = [0; DATA_HEADER + 1];
    data_header(&mut legacy, [0; 4], 1).unwrap();
    assert!(decode_for(&legacy, true).is_err());
}

#[test]
fn fabric_control_is_a_distinct_bounded_frame_on_the_negotiated_profile() {
    let mut bytes = [0; FABRIC_HEADER + CONTROL_FRAME_BYTES + 1];
    fabric_header(&mut bytes, 7, true).unwrap();
    assert_eq!(&bytes[..FABRIC_HEADER], &[2, 2, 0, 0, 0, 7]);
    let bounded = &bytes[..FABRIC_HEADER + CONTROL_FRAME_BYTES];
    assert!(
        matches!(decode_for(bounded, true), Ok(Frame::Data { context: Some(CONTROL_CONTEXT), attempt: 7, payload }) if payload.len() == CONTROL_FRAME_BYTES)
    );
    assert!(decode_for(&bytes, true).is_err());
    assert!(decode(bounded).is_err());
}

#[test]
fn feedback_preserves_received_and_missing_statuses_with_exact_bounded_lengths() {
    let mut bytes = [0u8; UDP_BUDGET];
    let header = feedback_header(&mut bytes, PacketId(u64::from(u32::MAX)), 13, 2).unwrap();
    let first = encode_status(&mut bytes[header..], Some(42)).unwrap();
    let second = encode_status(&mut bytes[header + first..], None).unwrap();
    let length = header + first + second;
    assert_eq!(
        &bytes[..length],
        &[
            1, 1, 0, 0, 0, 0, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 13, 0, 2, 1, 0, 0, 0, 0, 0,
            0, 0, 42, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]
    );
    let Ok(Frame::Feedback {
        base,
        generation,
        statuses,
    }) = decode(&bytes[..length])
    else {
        panic!("expected feedback frame");
    };
    assert_eq!(base, PacketId(u64::from(u32::MAX)));
    assert_eq!(generation, 13);
    let mut output = Vec::with_capacity(MAX_STATUSES);
    decode_statuses(statuses, &mut output).unwrap();
    assert!(output[0].received);
    assert_eq!(output[0].arrival, Some(ReceiverTime::Micros(42)));
    assert!(!output[1].received);
    assert_eq!(output[1].arrival, None);
    for size in 0..length {
        assert!(decode(&bytes[..size]).is_err());
    }
    assert!(decode(&bytes[..length + 1]).is_err());
    assert!(feedback_header(&mut bytes, PacketId(0), 0, MAX_STATUSES + 1).is_err());
    let mut malformed = [0u8; 9];
    malformed[0] = 2;
    assert!(decode_statuses(&malformed, &mut output).is_err());
}
