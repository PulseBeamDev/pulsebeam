use super::*;

pub(super) type ResourceNotification = Box<dyn FnOnce() + Send>;

macro_rules! resource_observations {
    ($($variant:ident => ($observer:ident, $projection:ident, $register:ident)),+ $(,)?) => {
        pub(super) enum ResourceObservation {
            $($variant {
                owner: u64,
                observer: std::sync::Weak<agent_core::observer::$observer>,
            }),+
        }

        impl ResourceObservation {
            fn alive(&self) -> bool {
                match self {
                    $(Self::$variant { observer, .. } => observer.strong_count() > 0),+
                }
            }

            fn prepare(&self, core: &agent_core::Agent, closing: bool) -> Option<ResourceNotification> {
                match self {
                    $(Self::$variant { owner, observer } => {
                        let observer = observer.upgrade()?;
                        let state = core.$projection(*owner);
                        let closing = closing || state.is_err();
                        let state = state.unwrap_or_else(|_| observer.state());
                        let notification = observer.prepare(state, closing).ok()?;
                        Some(Box::new(move || { let _ = notification.notify(); }))
                    }),+
                }
            }
        }

        impl Control {
            $(pub(in crate::native_actor) fn $register(
                &self,
                owner: u64,
            ) -> Result<Arc<agent_core::observer::$observer>, Error> {
                let core = self.core.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let observer = Arc::new(agent_core::observer::$observer::new(core.$projection(owner)?));
                self.resource_observers.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(ResourceObservation::$variant { owner, observer: Arc::downgrade(&observer) });
                Ok(observer)
            })+
        }
    };
}

resource_observations! {
    VideoSubscription => (VideoSubscriptionStateObserver, video_subscription_state, video_subscription_observer),
    AudioSubscription => (AudioSubscriptionStateObserver, audio_subscription_state, audio_subscription_observer),
    DataSubscription => (DataSubscriptionStateObserver, data_subscription_state, data_subscription_observer),
    DataPublication => (DataPublicationStateObserver, data_publication_state, data_publication_observer),
}

impl Control {
    pub(super) fn resource_notifications(
        &self,
        core: &agent_core::Agent,
        closing: bool,
    ) -> Vec<ResourceNotification> {
        let mut registry = self
            .resource_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(ResourceObservation::alive);
        let mut notifications = registry
            .iter()
            .filter_map(|observer| observer.prepare(core, closing))
            .collect::<Vec<_>>();
        drop(registry);
        notifications.extend(self.watch_notifications(core, closing));
        notifications
    }
}
