//! Version 1 reverse-control wire: version, reverse tag, big-endian body length, body.
//! The authenticated Link binding supplies the destination. Forward images use Fabric only.

use pulsebeam_link::{CodecError, WireCodec};

use crate::participant::reverse::ReversePacket;
use crate::route::{NodeRouteAddress, RouteId};
use crate::shard::worker::ShardFrame;

const PREFIX: [u8; 2] = [1, 0];
const MAX_WIRE: usize = 1200;
const HEADER: usize = 4;

pub(crate) struct ReverseCodec;

impl WireCodec<ShardFrame> for ReverseCodec {
    fn encoded_len(&self, frame: &ShardFrame) -> Result<usize, CodecError> {
        let ShardFrame::Reverse { packet, .. } = frame else {
            return Err(CodecError::Unsupported);
        };
        HEADER
            .checked_add(packet.wire().len())
            .filter(|len| *len <= MAX_WIRE)
            .ok_or(CodecError::TooLarge)
    }

    fn encode(&self, frame: &ShardFrame, dst: &mut [u8]) -> Result<usize, CodecError> {
        let ShardFrame::Reverse { packet, .. } = frame else {
            return Err(CodecError::Unsupported);
        };
        let size = self.encoded_len(frame)?;
        let len = u16::try_from(packet.wire().len()).map_err(|_| CodecError::TooLarge)?;
        let dst = dst.get_mut(..size).ok_or(CodecError::TooLarge)?;
        let (header, body) = dst
            .split_first_chunk_mut::<HEADER>()
            .ok_or(CodecError::TooLarge)?;
        let [version, tag] = PREFIX;
        let [high, low] = len.to_be_bytes();
        *header = [version, tag, high, low];
        body.copy_from_slice(packet.wire());
        Ok(size)
    }

    fn decode(&self, wire: &[u8]) -> Result<ShardFrame, CodecError> {
        if wire.len() > MAX_WIRE {
            return Err(CodecError::TooLarge);
        }
        let [1, 0, high, low, body @ ..] = wire else {
            return Err(CodecError::Malformed);
        };
        if body.len() != usize::from(u16::from_be_bytes([*high, *low])) {
            return Err(CodecError::Malformed);
        }
        let packet = ReversePacket::from_wire(body).ok_or(CodecError::Malformed)?;
        let address = NodeRouteAddress::new(RouteId::from_raw(0), 0);
        Ok(ShardFrame::Reverse { address, packet })
    }
}
