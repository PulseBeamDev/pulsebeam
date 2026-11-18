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
        reason = "only the control actor can originate result owners, so a sole cache reference can safely transfer its credit"
    )]
    fn reclaimable(&self) -> usize {
        if std::sync::Arc::strong_count(&self.0) == 1 {
            self.len()
        } else {
            0
        }
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

    pub fn can_reserve(&self, previous: Option<&CachedResult>) -> bool {
        self.0
            .available_permits()
            .checked_add(previous.map_or(0, |result| result.bytes.reclaimable()))
            .is_some_and(|available| available >= MAX_RESULT_BYTES)
    }

    #[allow(
        clippy::disallowed_types,
        reason = "the control actor reclaims only cache-exclusive HTTP allocations; external delivery owners prevent credit transfer"
    )]
    pub fn reserve(
        &self,
        previous: Option<&mut Option<CachedResult>>,
    ) -> Result<ResultReservation, Error> {
        let reclaim = previous
            .as_deref()
            .and_then(Option::as_ref)
            .map_or(0, |result| result.bytes.reclaimable());
        let additional = MAX_RESULT_BYTES
            .checked_sub(reclaim)
            .ok_or(Error::InvalidTransition)?;
        let credit = self
            .0
            .clone()
            .try_acquire_many_owned(u32::try_from(additional).map_err(|_| Error::Capacity)?)
            .map_err(|_| Error::Capacity)?;
        if reclaim == 0 {
            return Ok(ResultReservation(credit));
        }
        let Some(previous) = previous.and_then(Option::take) else {
            pulsebeam_runtime::fatal!("reclaimable HTTP result lost its allocation");
        };
        let Allocation {
            bytes,
            credit: mut reclaimed,
        } = std::sync::Arc::try_unwrap(previous.bytes.0).unwrap_or_else(|_| {
            pulsebeam_runtime::fatal!(
                "sole HTTP result acquired an external reference during reservation"
            )
        });
        reclaimed.merge(credit);
        drop(bytes);
        Ok(ResultReservation(reclaimed))
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
