#![allow(
    clippy::disallowed_types,
    reason = "NetworkInput is intentionally Bytes-backed"
)]

use bytes::Bytes;
use pulsebeam_rtc::{IceTcpFlowId, NetworkInput};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

#[test]
fn ice_tcp_input_carries_one_complete_frame_and_an_opaque_flow() {
    let local = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000));
    let input = NetworkInput::IceTcp {
        flow: IceTcpFlowId::from_value(9),
        local,
        remote: local,
        frame: Bytes::from_static(&[0, 1, 0]),
    };
    assert!(matches!(input, NetworkInput::IceTcp { .. }));
}
