use std::time::Instant;

pub struct ConnectionEntropy([u8; 32]);

impl ConnectionEntropy {
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn into_bytes(mut self) -> [u8; 32] {
        let bytes = self.0;
        self.0.fill(0);
        bytes
    }
}

pub(crate) struct MonotonicObserver {
    last: Instant,
    regressions: u64,
    warning_pending: bool,
}

impl MonotonicObserver {
    pub(crate) const fn starting_at(at: Instant) -> Self {
        Self {
            last: at,
            regressions: 0,
            warning_pending: false,
        }
    }

    pub(crate) fn observe(&mut self, at: Instant) -> Instant {
        if at < self.last {
            debug_assert!(false, "connection time regressed");
            self.regressions = self.regressions.saturating_add(1);
            self.warning_pending = true;
        }
        self.last = at.max(self.last);
        self.last
    }

    pub(crate) const fn regressions(&self) -> u64 {
        self.regressions
    }

    pub(crate) fn take_warning(&mut self) -> bool {
        std::mem::take(&mut self.warning_pending)
    }
}
