use std::sync::Arc;

use crate::{participant, sink, source};
use pulsebeam_runtime::actor;

#[derive(Clone)]
pub struct SystemContext {
    pub rng: pulsebeam_runtime::rand::Rng,
    pub source_handle: source::SourceHandle,
    pub sink_handle: sink::SinkHandle,
    pub participant_factory: Arc<Box<dyn actor::ActorFactory<participant::ParticipantActor>>>,
}
