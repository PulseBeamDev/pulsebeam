use super::*;
use crate::engine::{ResourceKind, ResourceNotification, ResourceState};
use agent_core::{ffi, observer};

macro_rules! resource_observations {
    ($($kind:ident => ($observer:ident, $typed:ident)),+ $(,)?) => {
        pub(super) enum ResourceObserver {
            $($kind(Rc<observer::$observer>)),+
        }

        impl ResourceObserver {
            pub(super) fn new(state: &ResourceState) -> Self {
                match state {
                    $(ResourceState::$kind(state) => Self::$kind(Rc::new(observer::$observer::new(state.clone())))),+
                }
            }

            pub(super) fn state(&self) -> ResourceState {
                match self { $(Self::$kind(observer) => ResourceState::$kind(observer.state())),+ }
            }

            pub(super) fn prepare(&self, state: &ResourceState, closing: bool) -> Option<ResourceNotification> {
                let data_state = match (self, state) {
                    (Self::DataSubscription(observer), ResourceState::DataSubscription(state)) => {
                        let terminal = observer.state().terminal_error.or_else(|| data_receive_failure(state));
                        if terminal == state.terminal_error {
                            None
                        } else {
                            let mut state = state.clone();
                            state.terminal_error = terminal;
                            Some(ResourceState::DataSubscription(state))
                        }
                    }
                    _ => None,
                };
                match (self, data_state.as_ref().unwrap_or(state)) {
                    $((Self::$kind(observer), ResourceState::$kind(state)) => {
                        let notification = observer.prepare(state.clone(), closing).ok()?;
                        Some(Box::new(move || { let _ = notification.notify(); }))
                    }),+
                    _ => None,
                }
            }
        }

        $(impl BrowserRuntime {
            pub(super) fn $typed(&self, owner: u64) -> Result<Rc<observer::$observer>, ffi::ErrorInfo> {
                let actor = self.inner.intent_actor()?;
                let state = actor.observe_resource(owner, ResourceKind::$kind)
                    .ok_or_else(|| ffi::ErrorInfo::new(ffi::ErrorCode::Closed, "Resource owner is not active"))?;
                let notification = self.inner.prepare_resource_state(owner, &state, false);
                let observer = match self.inner.resource_observers.borrow().get(&owner) {
                    Some(ResourceObserver::$kind(observer)) => observer.clone(),
                    _ => return Err(ffi::ErrorInfo::new(ffi::ErrorCode::Conflict, "Resource kind does not match")),
                };
                if let Some(notification) = notification { notification(); }
                Ok(observer)
            }
        })+
    };
}

resource_observations! {
    Video => (VideoSubscriptionStateObserver, video_state_observer),
    Audio => (AudioSubscriptionStateObserver, audio_state_observer),
    DataPublication => (DataPublicationStateObserver, data_publication_state_observer),
    DataSubscription => (DataSubscriptionStateObserver, data_subscription_state_observer),
    MediaCollection => (MediaCollectionObserver, media_collection_observer),
}

fn data_receive_failure(state: &ffi::DataSubscriptionState) -> Option<ffi::ErrorInfo> {
    state.terminal_error.clone().or_else(|| {
        if state.execution == ffi::PublicationExecution::Failed {
            state.execution_error.clone()
        } else if state.source_status == ffi::SubscriptionSourceStatus::Rejected
            || state.admission == ffi::PublicationAdmissionStatus::Rejected
        {
            state.error.clone()
        } else {
            None
        }
    })
}

impl RuntimeInner {
    pub(super) fn prepare_resource_state(
        &self,
        owner: u64,
        state: &ResourceState,
        closing: bool,
    ) -> Option<ResourceNotification> {
        if self.is_closed() && !closing {
            return None;
        }
        let mut observers = self.resource_observers.borrow_mut();
        let observer = observers
            .entry(owner)
            .or_insert_with(|| ResourceObserver::new(state));
        let notification = observer.prepare(state, closing);
        if closing {
            observers.remove(&owner);
        }
        notification
    }

    pub(super) fn prepare_current_resources(&self) -> Vec<ResourceNotification> {
        let states = self
            .actor
            .borrow()
            .as_ref()
            .map(ActorHandle::resource_states)
            .unwrap_or_default();
        states
            .iter()
            .filter_map(|(owner, state)| self.prepare_resource_state(*owner, state, false))
            .collect()
    }

    pub(super) fn prepare_resource_shutdown(&self) -> Vec<ResourceNotification> {
        let observers = std::mem::take(&mut *self.resource_observers.borrow_mut());
        observers
            .into_iter()
            .filter_map(|(owner, observer)| {
                let state = self
                    .actor
                    .borrow()
                    .as_ref()
                    .and_then(|actor| actor.observed_resource_state(owner))
                    .unwrap_or_else(|| observer.state());
                observer.prepare(&state, true)
            })
            .collect()
    }
}
