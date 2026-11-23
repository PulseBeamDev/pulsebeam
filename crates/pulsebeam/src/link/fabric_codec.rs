use std::num::NonZeroU32;

use pulsebeam_core::fabric::{WireError, wan::Window};
use pulsebeam_link::CodecError;

use crate::route::fabric::local::OwnedPacket;

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
