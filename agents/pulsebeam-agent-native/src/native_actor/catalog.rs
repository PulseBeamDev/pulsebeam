use super::*;
use agent_core::{
    TrackOwner,
    ffi::{WatchConfig, WatchState},
    observer::{ObserverLimit, WatchStateObserver, WatchUpdates},
};

struct WatchLease {
    owner: TrackOwner,
    core: Control,
    observer: Arc<WatchStateObserver>,
    closed: Signal,
}

impl Drop for WatchLease {
    fn drop(&mut self) {
        self.closed.close();
        self.core.close_catalog_watch(self.owner, &self.observer);
    }
}

#[derive(Clone)]
pub struct Watch {
    inner: Arc<WatchLease>,
}

impl Watch {
    pub(super) fn new(room: String, config: WatchConfig, core: Control) -> Result<Self, Error> {
        let (owner, observer) = core.open_catalog_watch(room, config)?;
        Ok(Self {
            inner: Arc::new(WatchLease {
                owner,
                core,
                observer,
                closed: Signal::default(),
            }),
        })
    }

    pub fn state(&self) -> WatchState {
        self.inner.observer.state()
    }

    pub fn updates(&self) -> Result<WatchUpdates, ObserverLimit> {
        self.inner.observer.updates()
    }

    pub fn close(&self) {
        if !self.inner.closed.is_closed() {
            self.inner.closed.close();
            self.inner
                .core
                .close_catalog_watch(self.inner.owner, &self.inner.observer);
        }
    }
}

#[cfg(test)]
mod tests;
