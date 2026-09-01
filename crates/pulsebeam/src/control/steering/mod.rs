#[cfg_attr(feature = "sim", path = "sim.rs")]
#[cfg_attr(not(feature = "sim"), path = "ebpf.rs")]
mod imp;

use anyhow::Result;
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
    imp::attach(sockets)
}
