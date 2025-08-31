use std::net::SocketAddr;

use crate::{sink, source};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::{actor, net, rand};
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct SystemContext {
    pub rng: pulsebeam_runtime::rand::Rng,
    pub source_handle: source::SourceHandle,
    pub sink_handle: sink::SinkHandle,
}

impl SystemContext {
    pub fn spawn(external_addr: SocketAddr, socket: net::UnifiedSocket) -> (Self, JoinHandle<()>) {
        let rng = rand::Rng::from_os_rng();
        let source_actor = source::SourceActor::new(external_addr, socket.clone());
        let sink_actor = sink::SinkActor::new(socket.clone());

        // TODO: tune capacity
        let (source_handle, source_join) =
            actor::spawn(source_actor, actor::RunnerConfig::default());
        let (sink_handle, sink_join) = actor::spawn(sink_actor, actor::RunnerConfig::default());

        let task = tokio::spawn(async {
            tokio::join!(source_join, sink_join);
        });
        let ctx = SystemContext {
            rng,
            source_handle,
            sink_handle,
        };
        (ctx, task)
    }
}
