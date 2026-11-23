use pulsebeam_link::{CodecError, WireCodec};

use crate::participant::reverse::ReversePacket;
use crate::route::{NodeRouteAddress, RouteId};
use crate::shard::worker::ShardFrame;

use super::ReverseCodec;

#[test]
fn preserves_reverse_wire_and_rejects_superseded_forward_tags() {
    let codec = ReverseCodec;
    let address = NodeRouteAddress::new(RouteId::from_raw(1), 2);
    let frame = ShardFrame::Reverse {
        address,
        packet: ReversePacket::keyframe(None, crate::media::KeyframeRequestKind::Pli),
    };
    let golden = [1, 0, 0, 3, 1, 1, 0];
    assert_eq!(codec.encoded_len(&frame).unwrap(), golden.len());
    let mut wire = [0; 7];
    assert_eq!(codec.encode(&frame, &mut wire).unwrap(), wire.len());
    assert_eq!(wire, golden);

    let ShardFrame::Reverse { address, packet } = codec.decode(&golden).unwrap() else {
        panic!("expected reverse control");
    };
    assert_eq!(address, NodeRouteAddress::new(RouteId::from_raw(0), 0));
    assert_eq!(packet.wire(), &golden[4..]);
    for len in 0..golden.len() {
        assert!(matches!(
            codec.decode(&golden[..len]),
            Err(CodecError::Malformed)
        ));
    }
    for tag in [1, 2] {
        let mut wire = golden;
        wire[1] = tag;
        assert!(matches!(codec.decode(&wire), Err(CodecError::Malformed)));
    }
    assert!(matches!(
        codec.decode(&[1, 0, 0, 3, 2, 7, 8]),
        Err(CodecError::Malformed)
    ));
}

#[test]
fn rejects_reverse_bodies_exceeding_the_envelope_budget() {
    assert!(matches!(
        ReverseCodec.decode(&[0; 1201]),
        Err(CodecError::TooLarge)
    ));
}
