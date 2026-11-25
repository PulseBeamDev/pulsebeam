use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};

/// Authorization for one live window. Renewal must not revive expired authority
/// or change the window's identity.
#[derive(Clone, Debug)]
pub struct LiveDeadline(Arc<Expiry>);

#[derive(Debug)]
struct Expiry {
    initial: Instant,
    offset_ns: AtomicI64,
}

impl LiveDeadline {
    pub fn new(expires: Instant) -> Self {
        Self(Arc::new(Expiry {
            initial: expires,
            offset_ns: AtomicI64::new(0),
        }))
    }

    pub fn expires(&self) -> Instant {
        // Only offsets whose reconstruction was checked are ever published.
        self.at(self.0.offset_ns.load(Ordering::Acquire))
            .unwrap_or(self.0.initial)
    }

    fn at(&self, offset: i64) -> Option<Instant> {
        let duration = Duration::from_nanos(offset.unsigned_abs());
        if offset < 0 {
            self.0.initial.checked_sub(duration)
        } else {
            self.0.initial.checked_add(duration)
        }
    }

    /// Apply the bound from the currently selected authority. It can shorten
    /// when aggregated demand loses the child that supplied the newest permit.
    pub fn renew(&self, now: Instant, expires: Instant) -> bool {
        if now >= expires {
            return false;
        }
        let initial = self.0.initial;
        let distance = expires
            .saturating_duration_since(initial)
            .max(initial.saturating_duration_since(expires));
        let Ok(offset) = i64::try_from(distance.as_nanos()) else {
            return false;
        };
        let offset = if expires < initial {
            offset.saturating_neg()
        } else {
            offset
        };
        if self.at(offset) != Some(expires) {
            return false;
        }
        self.0
            .offset_ns
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                self.at(current)
                    .filter(|&deadline| now < deadline)
                    .map(|_| offset)
            })
            .is_ok()
    }
}
