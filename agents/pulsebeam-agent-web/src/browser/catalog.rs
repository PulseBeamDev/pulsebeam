use super::*;
use crate::engine::ResourceNotification;
use agent_core::{ffi, observer};

pub(super) struct CatalogObserver {
    observer: observer::WatchStateObserver,
}

impl CatalogObserver {
    fn new(state: ffi::WatchState) -> Self {
        Self {
            observer: observer::WatchStateObserver::new(state),
        }
    }

    fn prepare(&self, state: ffi::WatchState, closing: bool) -> Option<ResourceNotification> {
        let notification = self.observer.prepare(state, closing).ok()?;
        Some(Box::new(move || {
            let _ = notification.notify();
        }))
    }
}

pub(crate) struct BrowserCatalogWatch {
    owner: u64,
    runtime: Weak<RuntimeInner>,
    shared: Rc<CatalogObserver>,
    closed: Cell<bool>,
}

impl BrowserCatalogWatch {
    pub(crate) fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        let notification = if let Some(runtime) = self.runtime.upgrade() {
            let state = runtime
                .actor
                .borrow()
                .as_ref()
                .and_then(|actor| actor.close_catalog_watch(self.owner))
                .unwrap_or_else(|| self.shared.observer.state());
            runtime.catalog_observers.borrow_mut().remove(&self.owner);
            self.shared.prepare(state, true)
        } else {
            self.shared.prepare(self.shared.observer.state(), true)
        };
        if let Some(notification) = notification {
            notification();
        }
    }
}

impl BrowserCatalogWatch {
    fn refresh(&self) {
        if let Some(runtime) = self.runtime.upgrade() {
            for notification in runtime.prepare_current_catalogs() {
                notification();
            }
        }
    }

    #[cfg(feature = "uniffi")]
    pub(crate) fn current_state(&self) -> ffi::WatchState {
        self.refresh();
        self.shared.observer.state()
    }

    #[cfg(feature = "uniffi")]
    pub(crate) fn open_updates(&self) -> Result<observer::WatchUpdates, ffi::ErrorInfo> {
        self.refresh();
        self.shared.observer.updates().map_err(|_| {
            ffi::ErrorInfo::new(
                ffi::ErrorCode::CapacityExceeded,
                "Watch observer capacity exceeded",
            )
        })
    }
}

impl Drop for BrowserCatalogWatch {
    fn drop(&mut self) {
        self.close();
    }
}

impl BrowserRuntime {
    pub(crate) fn open_watch(
        &self,
        room: String,
        options: ffi::WatchConfig,
    ) -> Result<BrowserCatalogWatch, ffi::ErrorInfo> {
        let (owner, state) = self
            .inner
            .intent_actor()?
            .open_catalog_watch(room, options)?;
        let shared = Rc::new(CatalogObserver::new(state));
        self.inner
            .catalog_observers
            .borrow_mut()
            .insert(owner, shared.clone());
        Ok(BrowserCatalogWatch {
            owner,
            runtime: Rc::downgrade(&self.inner),
            shared,
            closed: Cell::new(false),
        })
    }
}

impl RuntimeInner {
    pub(super) fn prepare_catalog_state(
        &self,
        owner: u64,
        state: &ffi::WatchState,
        closing: bool,
    ) -> Option<ResourceNotification> {
        let mut observers = self.catalog_observers.borrow_mut();
        let observer = observers.get(&owner)?;
        let notification = observer.prepare(state.clone(), closing);
        if closing {
            observers.remove(&owner);
        }
        notification
    }

    fn prepare_current_catalogs(&self) -> Vec<ResourceNotification> {
        let states = self
            .actor
            .borrow()
            .as_ref()
            .map(ActorHandle::catalog_states)
            .unwrap_or_default();
        states
            .iter()
            .filter_map(|(owner, state, closing)| {
                self.prepare_catalog_state(*owner, state, *closing)
            })
            .collect()
    }

    pub(super) fn prepare_catalog_shutdown(&self) -> Vec<ResourceNotification> {
        let observers = std::mem::take(&mut *self.catalog_observers.borrow_mut());
        observers
            .into_iter()
            .filter_map(|(owner, observer)| {
                let state = self
                    .actor
                    .borrow()
                    .as_ref()
                    .and_then(|actor| actor.close_catalog_watch(owner))
                    .unwrap_or_else(|| observer.observer.state());
                observer.prepare(state, true)
            })
            .collect()
    }
}
