use std::sync::Arc;

use pulsebeam::node::NodeBuilder;
use pulsebeam_runtime::net;
use tokio_util::sync::CancellationToken;

pub fn add(a: u32, b: u32) -> u32 {
    a + b
}
