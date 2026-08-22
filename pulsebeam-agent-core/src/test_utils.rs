use std::time::Duration;

use crate::{ChannelKey, MonotonicTime, RequestId, TransportGeneration};

pub fn time(milliseconds: u64) -> MonotonicTime {
    MonotonicTime::from_millis(milliseconds)
}

pub fn generation(value: u64) -> TransportGeneration {
    TransportGeneration::new(value)
}

pub fn request(value: u64) -> RequestId {
    RequestId::new(value)
}

pub fn channel(value: &str) -> ChannelKey {
    ChannelKey::new(value)
}

pub fn reconnect_policy(
    max_attempts: u32,
    initial_delay: Duration,
    max_delay: Duration,
) -> crate::ReconnectPolicy {
    crate::ReconnectPolicy {
        max_attempts,
        initial_delay,
        max_delay,
    }
}
