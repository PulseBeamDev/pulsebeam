#![allow(
    clippy::disallowed_types,
    clippy::expect_used,
    reason = "fixture probes a removed RTP extension"
)]

mod support;

use pulsebeam_rtc::{RtpExtension, WriteError};
use support::{PeerFixture, rtp_write};

#[test]
fn playout_delay_is_not_negotiated_or_written() {
    const URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay";
    let mut fixture = PeerFixture::connected_video();
    assert_eq!(
        fixture
            .connection
            .rtp_egress_extension_id(&fixture.sender_mid, None, URI),
        None
    );
    let source = fixture.send_source(&[0x65, 0x88, 0x84]);
    let mut packet = rtp_write(source, 1);
    packet.extensions.push(RtpExtension {
        uri: URI.into(),
        value: vec![0, 0, 0].into(),
    });
    let rejected = fixture.write_rtp(packet).expect_err("removed extension");
    assert_eq!(rejected.reason, WriteError::InvalidRtp);
    let mut packet = rejected.packet;
    packet
        .extensions
        .retain(|extension| extension.uri.as_ref() != URI);
    fixture.write_rtp(packet).expect("ordinary video packet");
    let header = fixture.receive_video_wire_packet();
    assert_eq!(header.ext_vals.play_delay_min, None);
    assert_eq!(header.ext_vals.play_delay_max, None);
}
