#![allow(
    clippy::expect_used,
    reason = "authenticated transit fixtures stop at violated public invariants"
)]

mod support;

use pulsebeam_rtc::{Command, FrameDependencies, MaterializedMediaPacket};
use support::{PeerFixture, forwarded};

#[test]
fn authenticated_ingress_survives_materialization_and_public_egress() {
    let mut fixture = PeerFixture::connected();
    for id in 1..=2 {
        let source = fixture.send_source(b"opaque transit payload");
        let canonical = source.bytes().to_vec();
        let global = source.global_media_at();
        let uris = [
            "urn:ietf:params:rtp-hdrext:sdes:mid",
            "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
        ];
        let views = uris.map(|uri| source.extension(uri).map(<[u8]>::to_vec));
        let wire = source
            .materialize()
            .expect("materialize authenticated source")
            .into_bytes();
        drop(source);
        let restored = MaterializedMediaPacket::from_bytes(wire)
            .expect("decode owned relay bytes")
            .into_packet()
            .expect("restore local packet state");
        assert_eq!(restored.bytes().as_ref(), canonical);
        assert_eq!(restored.global_media_at(), global);
        for (uri, view) in uris.into_iter().zip(views) {
            assert_eq!(restored.extension(uri), view.as_deref());
        }
        let mut media = forwarded(restored, id);
        media.frame.dependencies = FrameDependencies::Unknown;
        let media = media.to_transit();
        assert_eq!(media.frame.dependencies, FrameDependencies::Unknown);
        fixture
            .connection
            .command(
                fixture.at(),
                Command::SendMedia {
                    sender: fixture.sender,
                    media,
                },
            )
            .expect("admit restored packet with unknown dependencies");
        let (_, payload) = fixture.receive_egress();
        assert_eq!(payload, b"opaque transit payload");
    }
}
