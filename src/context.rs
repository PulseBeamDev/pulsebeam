use crate::rng::Rng;
use crate::sink::UdpSinkHandle;
use crate::source::UdpSourceHandle;

#[derive(Clone)]
pub struct Context {
    pub rng: Rng,
    pub source: UdpSourceHandle,
    pub sink: UdpSinkHandle,
}
