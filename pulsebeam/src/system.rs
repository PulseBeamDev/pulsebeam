use crate::{sink, source};

#[derive(Clone)]
pub struct SystemContext {
    rng: pulsebeam_runtime::rng::Rng,
    source_handle: source::SourceHandle,
    sink_handle: sink::SinkHandle,
}
