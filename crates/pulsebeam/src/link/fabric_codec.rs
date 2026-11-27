use std::num::NonZeroU32;

use pulsebeam_core::fabric::{
    WireError,
    wan::{OmissionRun, Window},
};
use pulsebeam_link::CodecError;

use crate::route::fabric::local::OwnedPacket;

const WAN_RTP_TAG_BYTES: usize = 16;
const _: () = assert!(
    pulsebeam_core::fabric::MAX_WAN_RTP_BYTES + WAN_RTP_TAG_BYTES
        == pulsebeam_link::wan::MAX_DATAGRAM_BYTES
);

pub(crate) struct FabricWanCodec;

impl FabricWanCodec {
    pub(crate) fn encode_projected(
        &self,
        window: Window,
        position: u64,
        packet: &OwnedPacket,
        omitted: OmissionRun,
        dst: &mut [u8],
    ) -> Result<usize, CodecError> {
        window
            .write_projected(position, packet, omitted, dst)
            .map_err(to_codec_error)
    }

    pub(crate) fn decode_live(
        &self,
        window: Window,
        plaintext: &[u8],
        projection_selected: bool,
    ) -> Result<(u64, (OwnedPacket, Option<OmissionRun>)), CodecError> {
        let (position, packet, omitted) = if projection_selected {
            window.decode_projected(plaintext)
        } else {
            window
                .decode(plaintext)
                .map(|(position, packet)| (position, packet, None))
        }
        .map_err(to_codec_error)?;
        Ok((position, (packet.into_owned(), omitted)))
    }
}

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
        self.decode_live(window, plaintext, false)
            .map(|(position, (packet, _))| (position, packet))
    }
}

fn to_codec_error(error: WireError) -> CodecError {
    match error {
        WireError::HopExhausted => CodecError::Unsupported,
        WireError::Oversize | WireError::OutputTooSmall => CodecError::TooLarge,
        _ => CodecError::Malformed,
    }
}
