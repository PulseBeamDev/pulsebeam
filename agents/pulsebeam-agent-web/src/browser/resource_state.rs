use super::publication::{error_value, optional};
use super::*;
use crate::engine::{ResourceKind, ResourceNotification, ResourceState};
use agent_core::{ffi, observer};

macro_rules! resource_observations {
    ($($kind:ident => ($state:ident, $observer:ident, $updates:ident, $browser:ident, $cursor:ident, $method:ident, $value:ident)),+ $(,)?) => {
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
                match (self, state) {
                    $((Self::$kind(observer), ResourceState::$kind(state)) => {
                        let notification = observer.prepare(state.clone(), closing).ok()?;
                        Some(Box::new(move || { let _ = notification.notify(); }))
                    }),+
                    _ => None,
                }
            }
        }

        $(#[wasm_bindgen]
        pub struct $browser { observer: Rc<observer::$observer> }

        #[wasm_bindgen]
        impl $browser {
            pub fn state(&self) -> JsValue { $value(&self.observer.state()) }
            pub fn updates(&self) -> Result<$cursor, JsValue> {
                self.observer.updates().map(|updates| $cursor { updates }).map_err(|_| {
                    operation_error_value(ffi::ErrorInfo::new(ffi::ErrorCode::CapacityExceeded, "Resource observer capacity exceeded"))
                })
            }
        }

        #[wasm_bindgen]
        pub struct $cursor { updates: observer::$updates }

        #[wasm_bindgen]
        impl $cursor {
            pub async fn next(&self) -> Result<JsValue, JsValue> {
                let next = self.updates.next().map_err(|_| {
                    operation_error_value(ffi::ErrorInfo::new(ffi::ErrorCode::Conflict, "A resource-state read is already pending"))
                })?;
                Ok(next.await.as_ref().map_or(JsValue::UNDEFINED, $value))
            }
            pub fn close(&self) { self.updates.close(); }
        }

        #[wasm_bindgen]
        impl BrowserRuntime {
            pub fn $method(&self, owner: u64) -> Result<$browser, JsValue> {
                self.inner.ensure_open()?;
                let state = self.inner.actor.borrow().as_ref()
                    .and_then(|actor| actor.observe_resource(owner, ResourceKind::$kind))
                    .ok_or_else(|| operation_error_value(ffi::ErrorInfo::new(ffi::ErrorCode::Closed, "Resource owner is not active")))?;
                let notification = self.inner.prepare_resource_state(owner, &state, false);
                let observer = match self.inner.resource_observers.borrow().get(&owner) {
                    Some(ResourceObserver::$kind(observer)) => observer.clone(),
                    _ => return Err(operation_error_value(ffi::ErrorInfo::new(ffi::ErrorCode::Conflict, "Resource kind does not match"))),
                };
                if let Some(notification) = notification { notification(); }
                Ok($browser { observer })
            }
        })+
    };
}

resource_observations! {
    Video => (VideoSubscriptionState, VideoSubscriptionStateObserver, VideoSubscriptionUpdates, BrowserVideoSubscriptionObserver, BrowserVideoSubscriptionUpdates, video_subscription, video_value),
    Audio => (AudioSubscriptionState, AudioSubscriptionStateObserver, AudioSubscriptionUpdates, BrowserAudioSubscriptionObserver, BrowserAudioSubscriptionUpdates, audio_subscription, audio_value),
    DataPublication => (DataPublicationState, DataPublicationStateObserver, DataPublicationUpdates, BrowserDataPublicationObserver, BrowserDataPublicationUpdates, data_publication, data_publication_value),
    DataSubscription => (DataSubscriptionState, DataSubscriptionStateObserver, DataSubscriptionUpdates, BrowserDataSubscriptionObserver, BrowserDataSubscriptionUpdates, data_subscription, data_subscription_value),
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

macro_rules! resource_value {
    ($state:expr) => {{
        let state = $state;
        let value = Object::new();
        set(&value, "observationRevision", state.observation_revision);
        set(&value, "intentRevision", state.intent_revision);
        set(&value, "evaluatedRevision", state.evaluated_revision);
        set(
            &value,
            "acceptedRevision",
            optional(state.accepted_revision),
        );
        set(
            &value,
            "rejectedRevision",
            optional(state.rejected_revision),
        );
        set(&value, "localEnabled", state.local_enabled);
        set(&value, "closed", state.closed);
        set(&value, "admission", state.admission as u32);
        set(&value, "admissionPending", state.admission_pending);
        set(
            &value,
            "error",
            state.error.as_ref().map_or(JsValue::UNDEFINED, error_value),
        );
        set(&value, "execution", state.execution as u32);
        set(
            &value,
            "executionError",
            state
                .execution_error
                .as_ref()
                .map_or(JsValue::UNDEFINED, error_value),
        );
        value
    }};
}

macro_rules! subscription_value {
    ($state:expr) => {{
        let state = $state;
        let value = resource_value!(state);
        set(&value, "selector", selector_value(&state.selector));
        set(&value, "sourceStatus", state.source_status as u32);
        set(
            &value,
            "sourceTrackId",
            state
                .source_track_id
                .clone()
                .map_or(JsValue::UNDEFINED, JsValue::from),
        );
        value
    }};
}

fn selector_value(selector: &ffi::TrackSelector) -> JsValue {
    let value = Object::new();
    set(&value, "room", selector.room.clone());
    set(&value, "participantId", selector.participant_id.clone());
    set(&value, "trackId", selector.track_id.clone());
    value.into()
}

fn playout_value(delay: ffi::PlayoutDelay) -> JsValue {
    let value = Object::new();
    match delay {
        ffi::PlayoutDelay::Adaptive => set(&value, "tag", "Adaptive"),
        ffi::PlayoutDelay::Fixed { min_ms, max_ms } => {
            set(&value, "tag", "Fixed");
            let inner = Object::new();
            set(&inner, "minMs", min_ms);
            set(&inner, "maxMs", max_ms);
            set(&value, "inner", inner);
        }
    }
    value.into()
}

fn video_value(state: &ffi::VideoSubscriptionState) -> JsValue {
    let value = subscription_value!(state);
    let configuration = Object::new();
    set(&configuration, "visible", state.configuration.visible);
    set(&configuration, "height", state.configuration.height);
    set(&configuration, "minHeight", state.configuration.min_height);
    set(&configuration, "minFps", state.configuration.min_fps);
    set(&configuration, "priority", state.configuration.priority);
    set(
        &configuration,
        "playoutDelay",
        playout_value(state.configuration.playout_delay),
    );
    set(&value, "configuration", configuration);
    value.into()
}

fn audio_value(state: &ffi::AudioSubscriptionState) -> JsValue {
    let value = subscription_value!(state);
    set(&value, "playoutDelay", playout_value(state.playout_delay));
    value.into()
}

fn data_publication_value(state: &ffi::DataPublicationState) -> JsValue {
    let value = resource_value!(state);
    set(&value, "trackId", state.track_id.clone());
    set(&value, "delivery", state.delivery as u32);
    value.into()
}

fn data_subscription_value(state: &ffi::DataSubscriptionState) -> JsValue {
    let value = subscription_value!(state);
    set(&value, "delivery", state.delivery as u32);
    set(
        &value,
        "terminalError",
        state
            .terminal_error
            .as_ref()
            .map_or(JsValue::UNDEFINED, error_value),
    );
    value.into()
}
