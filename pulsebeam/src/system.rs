use std::net::SocketAddr;

use crate::gateway;
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::{actor, net, rand};
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct SystemContext {
    pub rng: pulsebeam_runtime::rand::Rng,
    pub gw_handle: gateway::GatewayHandle,
}

impl SystemContext {
    pub fn spawn(external_addr: SocketAddr, socket: net::UnifiedSocket) -> (Self, JoinHandle<()>) {
        let rng = rand::Rng::from_os_rng();
        let gw_actor = gateway::GatewayActor::new(external_addr, socket);

        // TODO: tune capacity
        let (gw_handle, gw_join) = actor::spawn(gw_actor, actor::RunnerConfig::default());
        let task = tokio::spawn(async {
            tokio::join!(gw_join);
        });
        let ctx = SystemContext { rng, gw_handle };
        (ctx, task)
    }
}
