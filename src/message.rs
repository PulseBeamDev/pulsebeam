use bytes::Bytes;
use std::net::SocketAddr;

#[derive(Debug)]
pub struct EgressUDPPacket {
    pub raw: Bytes,
    pub dst: SocketAddr,
}
