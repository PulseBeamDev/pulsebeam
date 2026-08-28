use crate::{
    context::AgentContext,
    effect::{DataChannelConfig, DataChannelEffect, Effects},
    id::Generation,
};

pub(super) enum SignalingEffect {
    Data(DataChannelEffect),
}

pub(super) struct Signaling<S> {
    state: S,
    generation: Generation,
}

pub(super) struct WaitingTransport {}

impl Signaling<WaitingTransport> {
    fn transport_connected(self, cx: &mut AgentContext) -> Signaling<WaitingChannel> {
        let generation = cx.dc_open(DataChannelConfig::reliable(
            proto::namespace::Signaling::Reliable.as_str().into(),
        ));

        Signaling {
            state: WaitingChannel {},
            generation,
        }
    }
}

pub(super) struct WaitingChannel {}

impl Signaling<WaitingChannel> {}
