#![allow(
    clippy::expect_used,
    reason = "authenticated transit fixtures stop at violated public invariants"
)]

mod support;

use std::time::Duration;

use pulsebeam_rtc::{
    Command, Event, FrameDependencies, MaterializedMediaPacket, Output, PacketObservation,
    TransmitKind,
};
use support::{PeerFixture, forwarded};

#[test]
fn authenticated_ingress_survives_materialization_and_public_egress() {
    let mut fixture = PeerFixture::connected();
    for id in 1..=2 {
        let (observed, input) = fixture.observe_source(b"opaque transit payload");
        let delay = Duration::from_millis(100);
        fixture.advance_clock_without_poll(delay);
        let execution = fixture.at();
        fixture
            .connection
            .receive(
                execution,
                PacketObservation {
                    at: observed,
                    provenance: id,
                },
                input,
            )
            .expect("delayed authenticated input");
        let source = (0..64)
            .find_map(|_| match fixture.connection.poll(execution) {
                Output::Event(Event::Media { packet, .. }) => Some(packet),
                Output::Transmit(transmit) => {
                    assert_eq!(transmit.kind, TransmitKind::Control);
                    None // Whole UDP control datagrams are deliberately dropped as local loss.
                }
                Output::Event(_) => None,
                output => panic!("authenticated source did not remain ready: {output:?}"),
            })
            .expect("authenticated delayed source");
        let origin = source.origin();
        assert_eq!(origin.observed_at, observed.global);
        assert_eq!(origin.provenance, id);
        if id == 1 {
            assert_eq!(source.global_media_at(), observed.global);
        }
        assert_eq!(fixture.connection.stats().connection.clock_regressions, 0);
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
        assert_eq!(restored.origin(), origin);
        for (uri, view) in uris.into_iter().zip(views) {
            assert_eq!(restored.extension(uri), view.as_deref());
        }
        let mut media = forwarded(restored, id);
        media.frame.dependencies = FrameDependencies::Unknown;
        let media = media.to_transit();
        assert_eq!(media.frame.dependencies, FrameDependencies::Unknown);
        assert_eq!(media.packet.origin(), origin);
        fixture.record_transmits();
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
        let observations = fixture.take_transmit_observations();
        let (emitted, info) = observations
            .iter()
            .find_map(|(at, kind)| {
                if let TransmitKind::Original(info) = kind {
                    Some((*at, *info))
                } else {
                    None
                }
            })
            .expect("classified original media");
        assert_eq!(info.sender, fixture.sender);
        assert_eq!(info.origin, origin);
        assert_eq!(info.global_media_at, global);
        assert!(
            emitted
                .global
                .checked_duration_since(origin.observed_at)
                .expect("forward latency")
                >= delay
        );
    }
}
