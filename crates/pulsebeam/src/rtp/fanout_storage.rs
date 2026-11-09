use super::{RtpExtensions, RtpPacket};

#[test]
fn transit_shares_immutable_storage_without_copying_recipient_payloads() {
    let packet = RtpPacket {
        ext_vals: RtpExtensions {
            opaque: vec![pulsebeam_rtc::RtpExtension {
                uri: "urn:example:opaque".into(),
                value: bytes::Bytes::from_static(&[1, 2, 3]),
            }]
            .into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let first = packet.to_transit();
    let second = packet.to_transit();
    assert_eq!(packet.payload.as_ptr(), first.payload.as_ptr());
    assert_eq!(first.payload.as_ptr(), second.payload.as_ptr());
    assert_eq!(
        packet.ext_vals.opaque.as_ptr(),
        first.ext_vals.opaque.as_ptr()
    );
    assert_eq!(
        first.ext_vals.opaque.as_ptr(),
        second.ext_vals.opaque.as_ptr()
    );
    assert_eq!(packet.arrival_ts, first.arrival_ts);
    assert_eq!(packet.playout_time, second.playout_time);
}
