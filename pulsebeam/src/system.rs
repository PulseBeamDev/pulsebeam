use crate::{sink, source};

#[derive(Clone)]
pub struct SystemContext {
    pub rng: pulsebeam_runtime::rand::Rng,
    pub source_handle: source::SourceHandle,
    pub sink_handle: sink::SinkHandle,
}
