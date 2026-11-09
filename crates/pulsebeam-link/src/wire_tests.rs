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
    data_header(&mut bytes, 0x01020304, u32::MAX).unwrap();
    bytes[DATA_HEADER..].copy_from_slice(&[9, 8, 7]);
    for length in 0..DATA_HEADER {
        assert!(decode(&bytes[..length]).is_err());
    }
    assert!(matches!(
        decode(&bytes),
        Ok(Frame::Data {
            route: 0x01020304,
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
    data_header(&mut oversize, 1, 1).unwrap();
    assert!(decode(&oversize).is_err());
}

#[test]
fn feedback_preserves_received_and_missing_statuses_with_exact_bounded_lengths() {
    let mut bytes = [0u8; UDP_BUDGET];
    let header = feedback_header(&mut bytes, PacketId(u64::from(u32::MAX)), 13, 2).unwrap();
    let first = encode_status(&mut bytes[header..], Some(42)).unwrap();
    let second = encode_status(&mut bytes[header + first..], None).unwrap();
    let length = header + first + second;
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
