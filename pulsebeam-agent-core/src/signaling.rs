use crate::{
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
    fn transport_connected(
        self,
        effects: &mut Effects<SignalingEffect>,
    ) -> Signaling<WaitingChannel> {
        effects.emit(SignalingEffect::Data(DataChannelEffect::Create {
            generation: self.generation,
            config: DataChannelConfig::reliable(
                pulsebeam_proto::namespace::Signaling::Reliable
                    .as_str()
                    .into(),
            ),
        }));

        Signaling {
            state: WaitingChannel {},
            generation: self.generation,
        }
    }
}

pub(super) struct WaitingChannel {}

impl Signaling<WaitingChannel> {}
