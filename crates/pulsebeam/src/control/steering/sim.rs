use pulsebeam_runtime::net::BoundUdpSocket;

use crate::control::steering::Steering;
pub struct SimSteering;

impl Steering for SimSteering {
    fn pin_flow_to_owner(
        &mut self,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        shard: u16,
    ) {
        pulsebeam_runtime::net::install_steering_flow(source, destination, shard);
    }
}

pub fn attach(_sockets: &[BoundUdpSocket]) -> anyhow::Result<Box<dyn Steering>> {
    Ok(Box::new(SimSteering {}))
}
