use crate::{
    context::{AgentContext, DataChannelConfig, DataChannelEffect},
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
        cx.dc_open(
            self.generation,
            DataChannelConfig::reliable(proto::namespace::Signaling::Reliable.as_str().into()),
        );

        Signaling {
            state: WaitingChannel {},
            generation: self.generation,
        }
    }
}

pub(super) struct WaitingChannel {}

impl Signaling<WaitingChannel> {}
