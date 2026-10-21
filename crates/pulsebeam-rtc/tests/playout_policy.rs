#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    reason = "native fixture asserts explicit extension bytes, not playback policy"
)]

mod support;

use pulsebeam_rtc::{PlayoutDelay, RtpExtension};
use support::{PeerFixture, rtp_write};

#[test]
fn omitted_zero_and_explicit_playout_bounds_remain_caller_owned_wire_values() {
    let mut fixture = PeerFixture::connected_video();
    let source = fixture.send_source(&[0x65, 0x88, 0x84]);
    for (index, delay) in [
        None,
        Some(PlayoutDelay::ZERO),
        Some(PlayoutDelay::from_ticks(2, 40).unwrap()),
    ]
    .into_iter()
    .enumerate()
    {
        let mut packet = rtp_write(source.clone(), u64::try_from(index).unwrap() + 1);
        packet.extensions.retain(|value| {
            value.uri.as_ref() != "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"
        });
        if let Some(delay) = delay {
            packet.extensions.push(RtpExtension {
                uri: "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay".into(),
                value: delay.to_bytes().to_vec().into(),
            });
        }
        fixture.write_rtp(packet).expect("explicit RTP extension");
        let header = fixture.receive_video_wire_packet();
        assert_eq!(header.ext_vals.play_delay_min, delay.map(PlayoutDelay::min));
        assert_eq!(header.ext_vals.play_delay_max, delay.map(PlayoutDelay::max));
    }
}
