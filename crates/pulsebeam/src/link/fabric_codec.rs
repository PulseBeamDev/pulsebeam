//! Fabric v1 images on a negotiated association. No legacy SFU envelope.

use std::num::NonZeroU32;

use pulsebeam_core::fabric::{BorrowedPacket, WireError, wan::Window};
use pulsebeam_link::{
    BindError, BindingId, CodecError, DeliveryTag, FabricCodec, LinkControl, NetworkId,
    NodeRouteKey, PortId,
};

use crate::route::fabric::local::{Emission, OwnedPacket};
use crate::shard::worker::ShardFrame;

pub(crate) struct FabricWireCodec;

/// Stages the route's agreed wire profile. The routing authority must supply
/// the source's full emission set and activate only after child installation.
pub(crate) fn stage_receive(
    link: &mut LinkControl<ShardFrame>,
    from: NetworkId,
    shard: u8,
    children: &[(PortId, DeliveryTag)],
    version: (u8, u8),
    emission: Emission,
) -> Result<(BindingId, NodeRouteKey), BindError> {
    let (major, revision) = version;
    if !emission.fits_registry(major, revision) {
        return Err(BindError::UnsupportedProfile);
    }
    link.stage_fabric_receive(
        from,
        shard,
        children,
        move |packet| matches!(packet, ShardFrame::FabricImage(image) if emission.accepts(image)),
    )
}

impl FabricCodec<OwnedPacket> for FabricWireCodec {
    fn encoded_len(&self, packet: &OwnedPacket) -> Result<usize, CodecError> {
        if packet.remaining_hops() == 0 {
            return Err(CodecError::Unsupported);
        }
        packet.legacy_wire_len().map_err(to_codec_error)
    }

    fn encode(
        &self,
        packet: &OwnedPacket,
        key: NodeRouteKey,
        dst: &mut [u8],
    ) -> Result<usize, CodecError> {
        let key =
            NonZeroU32::new(u32::from_be_bytes(key.to_bytes())).ok_or(CodecError::Malformed)?;
        packet.write_network(key, dst).map_err(to_codec_error)
    }

    fn route_key(&self, image: &[u8]) -> Result<NodeRouteKey, CodecError> {
        BorrowedPacket::network_route_key(image)
            .map(|key| NodeRouteKey::from_bytes(key.get().to_be_bytes()))
            .map_err(to_codec_error)
    }

    fn decode(&self, image: &[u8]) -> Result<OwnedPacket, CodecError> {
        BorrowedPacket::decode_network(image)
            .map(|(packet, _)| packet.into_owned())
            .map_err(to_codec_error)
    }
}

impl FabricCodec<ShardFrame> for FabricWireCodec {
    fn encoded_len(&self, frame: &ShardFrame) -> Result<usize, CodecError> {
        let ShardFrame::FabricImage(packet) = frame else {
            return Err(CodecError::Unsupported);
        };
        self.encoded_len(packet)
    }

    fn encode(
        &self,
        frame: &ShardFrame,
        key: NodeRouteKey,
        dst: &mut [u8],
    ) -> Result<usize, CodecError> {
        let ShardFrame::FabricImage(packet) = frame else {
            return Err(CodecError::Unsupported);
        };
        self.encode(packet, key, dst)
    }

    fn encode_positioned(
        &self,
        frame: &ShardFrame,
        key: NodeRouteKey,
        position: Option<u64>,
        dst: &mut [u8],
    ) -> Result<usize, CodecError> {
        let ShardFrame::FabricImage(packet) = frame else {
            return Err(CodecError::Unsupported);
        };
        if let Some(position) = position {
            let [_, _, _, _, a, b, c, d] = position.to_be_bytes();
            if u32::from_be_bytes([a, b, c, d]) != packet.header().sequence {
                return Err(CodecError::Malformed);
            }
        }
        self.encode(packet, key, dst)
    }

    fn route_key(&self, image: &[u8]) -> Result<NodeRouteKey, CodecError> {
        <Self as FabricCodec<OwnedPacket>>::route_key(self, image)
    }

    fn decode(&self, image: &[u8]) -> Result<ShardFrame, CodecError> {
        <Self as FabricCodec<OwnedPacket>>::decode(self, image).map(ShardFrame::FabricImage)
    }
}

const WAN_RTP_TAG_BYTES: usize = 16;
const _: () = assert!(
    pulsebeam_core::fabric::MAX_WAN_RTP_BYTES + WAN_RTP_TAG_BYTES
        == pulsebeam_link::wan::MAX_DATAGRAM_BYTES
);

pub(crate) struct FabricWanCodec;

impl pulsebeam_link::wan::Codec<OwnedPacket> for FabricWanCodec {
    type Window = Window;

    fn route(&self, window: Window) -> (NonZeroU32, u64) {
        (window.ssrc(), window.start())
    }

    fn encode(
        &self,
        window: Window,
        position: u64,
        packet: &OwnedPacket,
        dst: &mut [u8],
    ) -> Result<usize, CodecError> {
        window.write(position, packet, dst).map_err(|error| {
            #[cfg(feature = "sim")]
            if packet.header().profile == pulsebeam_core::fabric::Profile::Opus {
                crate::sim_metrics::record_routing_counter("fabric_wan_opus_encode_rejected");
                tracing::debug!(?error, source_bytes = ?packet.source_protected_bytes(), plaintext_bytes = packet.wan_plaintext_len(), "WAN Opus encoding rejected");
            }
            to_codec_error(error)
        })
    }

    fn decode(&self, window: Window, plaintext: &[u8]) -> Result<(u64, OwnedPacket), CodecError> {
        window
            .decode(plaintext)
            .map(|(position, packet)| (position, packet.into_owned()))
            .map_err(to_codec_error)
    }
}

fn to_codec_error(error: WireError) -> CodecError {
    match error {
        WireError::HopExhausted => CodecError::Unsupported,
        WireError::Oversize | WireError::OutputTooSmall => CodecError::TooLarge,
        _ => CodecError::Malformed,
    }
}

#[cfg(test)]
mod tests {
    use pulsebeam_core::fabric::{FabricPacket, Header, MAX_PACKET_BYTES, Profile};

    use super::*;

    #[test]
    fn fabric_network_codec_preserves_the_image_on_same_hop_retry() {
        const CORE_ENVELOPE: usize = 24;
        let packet = FabricPacket::new(
            Header {
                profile: Profile::Opus,
                marker: true,
                sequence: 3,
                timestamp: 48_000,
                ingress_ntp32: 7,
            },
            &[],
            Box::<[u8]>::from([0xa5_u8; MAX_PACKET_BYTES - CORE_ENVELOPE]),
        )
        .unwrap();
        let key = NodeRouteKey::from_bytes([3, 1, 0, 7]);
        let mut local = [0; MAX_PACKET_BYTES];
        assert_eq!(packet.write_local(&mut local), Ok(MAX_PACKET_BYTES));
        assert_eq!(
            FabricCodec::<OwnedPacket>::route_key(&FabricWireCodec, &local),
            Err(CodecError::Malformed)
        );
        let mut first = [0; MAX_PACKET_BYTES];
        let mut retry = [0; MAX_PACKET_BYTES];
        assert_eq!(
            FabricWireCodec.encode(&packet, key, &mut first),
            Ok(MAX_PACKET_BYTES)
        );
        assert_eq!(
            FabricWireCodec.encode(&packet, key, &mut retry),
            Ok(MAX_PACKET_BYTES)
        );
        assert_eq!(first, retry);
        assert_eq!(
            FabricCodec::<OwnedPacket>::route_key(&FabricWireCodec, &first),
            Ok(key)
        );
        let received = FabricCodec::<OwnedPacket>::decode(&FabricWireCodec, &first).unwrap();
        assert!(received.same_content(&packet));
        assert_eq!(received.remaining_hops(), 6);
        assert_eq!(packet.remaining_hops(), 7);
    }
}
