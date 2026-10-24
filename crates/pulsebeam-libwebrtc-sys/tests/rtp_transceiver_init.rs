//! Standalone init-value ownership and complete child-value contracts.
use pulsebeam_webrtc_sys::*;
use std::{sync::Arc, thread};

fn send_sync<T: Send + Sync>() {}

#[test]
fn init_defaults_copy_assignment_and_complete_owned_children() {
    send_sync::<RtpTransceiverInit>();
    let mut init = RtpTransceiverInit::default();
    assert_eq!(init.direction(), RtpTransceiverDirectionValue::SendReceive);
    assert!(init.stream_ids().is_empty());
    assert!(init.send_encodings().is_empty());
    init.set_direction(RtpTransceiverDirectionValue::Other(i32::MIN));
    let ids = vec![vec![], b"stream\0\xff".to_vec(), b"stream\0\xff".to_vec()];
    init.set_stream_ids(&ids);
    let mut encoding = RtpEncodingParameters::default();
    encoding.set_rid(b"rid\0\xff");
    encoding.set_scalability_mode(Some(b"unknown\0\xfe"));
    encoding.set_request_key_frame(true);
    encoding.set_bitrate_priority(f64::from_bits(0x7ff8_0000_0000_1234));
    encoding.set_network_priority(Priority::Other(i32::MIN));
    encoding.set_max_framerate(Some(f64::INFINITY));
    let mut resolution = Resolution::new(i32::MAX, i32::MAX);
    encoding.set_scale_resolution_down_to(Some(&resolution));
    init.set_send_encodings(&[encoding.clone(), encoding.clone()]);
    // Child objects and caller storage are independent, not borrowed overlays.
    resolution.set_width(1);
    encoding.set_request_key_frame(false);
    encoding.set_rid(b"caller-mutated");
    let copy = init.clone();
    let mut assigned = RtpTransceiverInit::default();
    assigned.clone_from(&copy);
    init.set_direction(RtpTransceiverDirectionValue::Stopped);
    init.set_stream_ids(&[]);
    init.set_send_encodings(&[]);
    drop(init);
    drop(encoding);
    drop(copy);
    assert_eq!(assigned.direction().raw(), i32::MIN);
    assert_eq!(assigned.stream_ids(), ids);
    let mut children = assigned.send_encodings();
    assert_eq!(children.len(), 2);
    for child in &children {
        assert_eq!(child.rid_bytes(), b"rid\0\xff");
        assert_eq!(child.scalability_mode_bytes().unwrap(), b"unknown\0\xfe");
        assert!(child.request_key_frame());
        assert_eq!(child.bitrate_priority().to_bits(), 0x7ff8_0000_0000_1234);
        assert_eq!(child.network_priority(), Priority::Other(i32::MIN));
        assert_eq!(child.max_framerate(), Some(f64::INFINITY));
        assert_eq!(child.scale_resolution_down_to().unwrap().width(), i32::MAX);
    }
    children[0].set_rid(b"snapshot-mutated");
    assert_eq!(assigned.send_encodings()[0].rid_bytes(), b"rid\0\xff");
    let assigned = Arc::new(assigned);
    let other = assigned.clone();
    thread::spawn(move || {
        assert_eq!(other.direction().raw(), i32::MIN);
        assert_eq!(
            other.send_encodings()[1].bitrate_priority().to_bits(),
            0x7ff8_0000_0000_1234
        );
        assert_eq!(other.stream_ids()[1], b"stream\0\xff");
    })
    .join()
    .unwrap();
    drop(assigned);
    // Extracted encoding children keep no parent, peer or factory origin.
    assert_eq!(children[1].rid_bytes(), b"rid\0\xff");
}
