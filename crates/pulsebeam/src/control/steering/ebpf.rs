use crate::control::steering::Steering;
use anyhow::{Result, bail};
use pulsebeam_runtime::net::BoundUdpSocket;

/// Open-source builds keep the steering integration point but do not ship the
/// production eBPF backend. Proprietary builds provide that implementation.
pub fn attach(_sockets: &[BoundUdpSocket]) -> Result<Box<dyn Steering>> {
    bail!("eBPF steering backend is not included in this build")
}
