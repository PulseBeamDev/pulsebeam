#[cfg_attr(feature = "sim", path = "sim.rs")]
#[cfg_attr(not(feature = "sim"), path = "ebpf.rs")]
mod steering;

use anyhow::Result;
use pulsebeam_routing::steer::FlowKey;
use pulsebeam_runtime::net::BoundUdpSocket;

pub trait Steering: Send + Sync {
    fn pin_flow_to_owner(
        &mut self,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        shard: u16,
    );
}

pub fn attach(sockets: &[BoundUdpSocket]) -> Result<Box<dyn Steering>> {
    steering::attach(sockets)
}

fn flow_key(source: std::net::SocketAddr, destination: std::net::SocketAddr) -> FlowKey {
    let (src_addr, src_v6) = socket_addr_parts(source);
    let (dst_addr, dst_v6) = socket_addr_parts(destination);
    debug_assert_eq!(src_v6, dst_v6, "a UDP flow cannot mix address families");
    FlowKey {
        src_addr,
        dst_addr,
        src_port: source.port(),
        dst_port: destination.port(),
        is_ipv6: u8::from(src_v6),
        _pad: [0; 3],
    }
}

fn socket_addr_parts(addr: std::net::SocketAddr) -> ([u8; 16], bool) {
    match addr.ip() {
        std::net::IpAddr::V4(ip) => {
            let mut bytes = [0; 16];
            bytes[..4].copy_from_slice(&ip.octets());
            (bytes, false)
        }
        std::net::IpAddr::V6(ip) => (ip.octets(), true),
    }
}
