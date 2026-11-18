use std::fmt;

use pulsebeam_runtime::sync::{OwnedSemaphorePermit, Semaphore};

use crate::control::controller::ownership::ResultBytes;

// Matches the current 16 prepared materializations with four waiting callers each.
pub(crate) const MAX_DELIVERIES: usize = 64;

#[allow(
    clippy::disallowed_types,
    reason = "one node-local control/HTTP delivery quota, never read on the shard packet path"
)]
pub(crate) struct DeliveryBudget(std::sync::Arc<Semaphore>);

impl DeliveryBudget {
    #[allow(
        clippy::disallowed_types,
        reason = "initialize one finite control/HTTP delivery quota per controller"
    )]
    pub(crate) fn new() -> Self {
        Self(std::sync::Arc::new(Semaphore::new(MAX_DELIVERIES)))
    }

    #[allow(
        clippy::disallowed_types,
        reason = "reserve a delivery owner before admission without blocking the control actor"
    )]
    pub(crate) fn reserve(&self) -> Option<OwnedSemaphorePermit> {
        self.0.clone().try_acquire_owned().ok()
    }
}

pub(crate) struct Delivery {
    image: ResultBytes,
    _permit: OwnedSemaphorePermit,
}

impl Delivery {
    pub(crate) fn new(image: ResultBytes, permit: OwnedSemaphorePermit) -> Self {
        Self {
            image,
            _permit: permit,
        }
    }

    #[allow(
        clippy::disallowed_types,
        reason = "HTTP-only byte slices retain both delivery and allocation credit through the last consumer, with no cross-shard sharing"
    )]
    pub(super) fn into_bytes(self) -> bytes::Bytes {
        bytes::Bytes::from_owner(self)
    }
}

impl AsRef<[u8]> for Delivery {
    fn as_ref(&self) -> &[u8] {
        self.image.as_ref()
    }
}

impl fmt::Debug for Delivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Delivery([REDACTED])")
    }
}
