use std::fmt;

use pulsebeam_runtime::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

use super::{Error, MAX_RESULT_BYTES};

pub(super) struct CachedResult {
    pub bytes: ResultBytes,
    pub deadline: Instant,
}

struct Allocation {
    bytes: Box<[u8]>,
    credit: OwnedSemaphorePermit,
}

#[derive(Clone)]
#[allow(
    clippy::disallowed_types,
    reason = "immutable HTTP replay storage is shared only between control and HTTP delivery, never on the shard packet path"
)]
pub(crate) struct ResultBytes(std::sync::Arc<Allocation>);

impl ResultBytes {
    pub(super) fn len(&self) -> usize {
        self.0.bytes.len()
    }

    #[allow(
        clippy::disallowed_types,
        reason = "the HTTP byte owner retains allocation credit until the final cached or delivered byte reference drops"
    )]
    pub(crate) fn into_bytes(self) -> bytes::Bytes {
        bytes::Bytes::from_owner(self)
    }
}

impl AsRef<[u8]> for ResultBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0.bytes
    }
}

impl fmt::Debug for ResultBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResultBytes([REDACTED])")
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "control-only result allocation quota spans cache eviction and HTTP delivery lifetime, not media packets"
)]
pub(super) struct ResultBudget(std::sync::Arc<Semaphore>);

impl ResultBudget {
    #[allow(
        clippy::disallowed_types,
        reason = "create one control/HTTP allocation quota per authority, with no shard packet-path access"
    )]
    pub fn new(limit: usize) -> Self {
        Self(std::sync::Arc::new(Semaphore::new(limit)))
    }

    pub fn can_reserve(&self) -> bool {
        self.0.available_permits() >= MAX_RESULT_BYTES
    }

    #[allow(
        clippy::disallowed_types,
        reason = "a pending HTTP allocation owns its quota independently of the accepted image"
    )]
    pub fn reserve(&self) -> Result<ResultReservation, Error> {
        let permits = u32::try_from(MAX_RESULT_BYTES).map_err(|_| Error::Capacity)?;
        let credit = self
            .0
            .clone()
            .try_acquire_many_owned(permits)
            .map_err(|_| Error::Capacity)?;
        Ok(ResultReservation(credit))
    }
}

pub(super) struct ResultReservation(OwnedSemaphorePermit);

impl ResultReservation {
    #[allow(
        clippy::disallowed_types,
        reason = "publish one immutable HTTP result allocation whose credit follows cache and delivery references, never media packets"
    )]
    pub fn finish(mut self, bytes: Vec<u8>) -> ResultBytes {
        let bytes = bytes.into_boxed_slice();
        let Some(unused) = self.0.num_permits().checked_sub(bytes.len()) else {
            pulsebeam_runtime::fatal!("HTTP result exceeds its reserved allocation credit");
        };
        let released = self.0.split(unused).unwrap_or_else(|| {
            pulsebeam_runtime::fatal!("HTTP result allocation credit changed during commit")
        });
        drop(released);
        ResultBytes(std::sync::Arc::new(Allocation {
            bytes,
            credit: self.0,
        }))
    }
}
