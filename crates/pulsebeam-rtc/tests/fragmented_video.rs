#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "native wire fixtures stop at violated RTP invariants"
)]

mod support;

use support::{PeerFixture, rtp_write};

#[test]
fn direct_fragmented_video_preserves_caller_timing_markers_and_opaque_dd() {
    let mut fixture = PeerFixture::connected_video_dd();
    let sources = fixture.send_fragmented_video_source(Some(0));
    assert!(sources.len() >= 3);
    let timestamp = sources[0].header.timestamp;
    let mut expected_dd = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        assert_eq!(source.header.timestamp, timestamp);
        expected_dd.push(
            source
                .extensions
                .iter()
                .find(|value| value.uri.as_ref() == pulsebeam_core::dd::URI)
                .expect("native source emits negotiated DD")
                .value
                .to_vec(),
        );
        fixture
            .write_rtp(rtp_write(
                source.clone(),
                1_000 + u64::try_from(index).unwrap(),
            ))
            .expect("write fragment");
    }
    for (index, dd) in expected_dd.into_iter().enumerate() {
        let header = fixture.receive_video_wire_packet();
        assert_eq!(header.timestamp, timestamp);
        assert_eq!(
            header.sequence_number,
            1_000 + u16::try_from(index).unwrap()
        );
        assert_eq!(header.marker, index + 1 == sources.len());
        assert_eq!(header.ext_vals.dependency_descriptor, Some(dd));
        assert!(!header.padding);
    }
    let frame = fixture.receive_video_frame();
    assert!(
        frame.data.len() >= 3_500,
        "native peer authenticated and reassembled fragmented media"
    );
}
