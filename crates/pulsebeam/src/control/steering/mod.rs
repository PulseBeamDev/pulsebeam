use std::net::SocketAddr;

pub trait Steering: Send + Sync {
    fn pin_flow_to_owner(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        shard: u16,
    );
}

#[cfg(feature = "sim")]
pub(crate) mod sim;
