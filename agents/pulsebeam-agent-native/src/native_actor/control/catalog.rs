use super::*;
use agent_core::{
    TrackOwner,
    ffi::{WatchConfig, WatchState},
    observer::WatchStateObserver,
};

type RegisteredWatch = (TrackOwner, std::sync::Weak<WatchStateObserver>);
pub(super) type WatchObservers = Arc<std::sync::Mutex<BTreeMap<u64, RegisteredWatch>>>;

impl Control {
    pub(in crate::native_actor) fn open_catalog_watch(
        &self,
        room: String,
        config: WatchConfig,
    ) -> Result<(TrackOwner, Arc<WatchStateObserver>), Error> {
        let mut core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let owner = core.catalog_watch(room, config).map_err(Error::Operation)?;
        let state = core
            .catalog_watch_state(owner)
            .ok_or_else(|| Error::Rtc("Core watch state missing after admission".into()))?;
        let observer = Arc::new(WatchStateObserver::new(state));
        let mut registry = self
            .watch_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(|_, (_, observer)| observer.strong_count() > 0);
        registry.insert(owner.id(), (owner, Arc::downgrade(&observer)));
        drop(registry);
        drop(core);
        self.changed.notify_one();
        Ok((owner, observer))
    }

    pub(in crate::native_actor) fn close_catalog_watch(
        &self,
        owner: TrackOwner,
        observer: &WatchStateObserver,
    ) {
        let mut core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = core
            .close_catalog_watch(owner)
            .unwrap_or_else(|| observer.state());
        self.watch_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&owner.id());
        let notification = observer.prepare(state, true).ok();
        drop(core);
        if let Some(notification) = notification {
            let _ = notification.notify();
        }
        self.changed.notify_one();
    }

    pub(super) fn watch_notifications(
        &self,
        core: &agent_core::Agent,
        closing: bool,
    ) -> Vec<resource_state::ResourceNotification> {
        let mut registry = self
            .watch_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(|_, (_, observer)| observer.strong_count() > 0);
        registry
            .values()
            .filter_map(|(owner, weak)| {
                let observer = weak.upgrade()?;
                let state = match core.catalog_watch_state(*owner) {
                    Some(state) => state,
                    None if closing => observer.state(),
                    // Terminal state is carried by Core's one-shot notification, not inferred from absence.
                    None => return None,
                };
                let closing = closing || state.status == agent_core::ffi::WatchStatus::Failed;
                let notification = observer.prepare(state, closing).ok()?;
                Some(Box::new(move || {
                    let _ = notification.notify();
                }) as resource_state::ResourceNotification)
            })
            .collect()
    }

    pub(super) fn watch_terminated(&self, owner: u64, state: WatchState) {
        let observer = self
            .watch_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&owner)
            .and_then(|(_, weak)| weak.upgrade());
        if let Some(observer) = observer {
            if let Ok(notification) = observer.prepare(state, true) {
                let _ = notification.notify();
            }
        }
    }
}

#[cfg(test)]
mod tests;
