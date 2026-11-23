use std::rc::Rc;

use crate::engine::ActorHandle;

use js_sys::{BigInt, Object};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

use super::{operation_error_value, set};
use agent_core::{PublicationInputId, ffi, observer};

#[wasm_bindgen]
pub struct BrowserPublicationObserver {
    pub(super) observer: Rc<observer::PublicationStateObserver>,
}

#[wasm_bindgen]
impl BrowserPublicationObserver {
    pub fn state(&self) -> JsValue {
        publication_state_value(&self.observer.state())
    }

    pub fn updates(&self) -> Result<BrowserPublicationUpdates, JsValue> {
        self.observer
            .updates()
            .map(|updates| BrowserPublicationUpdates { updates })
            .map_err(|_| {
                operation_error_value(ffi::ErrorInfo::new(
                    ffi::ErrorCode::CapacityExceeded,
                    "Publication observer capacity exceeded",
                ))
            })
    }
}

#[wasm_bindgen]
pub struct BrowserPublicationUpdates {
    updates: observer::PublicationUpdates,
}

#[wasm_bindgen]
pub struct BrowserPipe {
    pub(super) actor: ActorHandle,
    pub(super) input: PublicationInputId,
    pub(super) observer: Rc<observer::PipeStateObserver>,
    pub(super) closed: std::cell::Cell<bool>,
    pub(super) owner: u64,
    pub(super) slot: String,
    pub(super) inner: std::rc::Weak<super::RuntimeInner>,
    pub(super) closing: std::cell::RefCell<Option<js_sys::Promise>>,
}

#[wasm_bindgen]
impl BrowserPipe {
    pub async fn install_track(
        &self,
        track: web_sys::MediaStreamTrack,
        config: JsValue,
    ) -> Result<(), JsValue> {
        let config: super::SenderConfig = serde_wasm_bindgen::from_value(config).map_err(|_| {
            operation_error_value(ffi::ErrorInfo::new(
                ffi::ErrorCode::InvalidArgument,
                "Invalid sender configuration",
            ))
        })?;
        self.install_input(track, config).await
    }
}

impl BrowserPipe {
    pub(crate) async fn install_input(
        &self,
        track: web_sys::MediaStreamTrack,
        config: super::SenderConfig,
    ) -> Result<(), JsValue> {
        if self.closed.get() || self.observer.state().status == ffi::PipeStatus::Closed {
            return Err(operation_error_value(ffi::ErrorInfo::new(
                ffi::ErrorCode::Closed,
                "Pipe is closed",
            )));
        }
        let inner = self.inner.upgrade().ok_or_else(|| {
            operation_error_value(ffi::ErrorInfo::new(
                ffi::ErrorCode::Closed,
                "Browser runtime is closed",
            ))
        })?;
        inner
            .replace_owned_input(
                Some(track),
                config,
                super::PublicationInputRequest {
                    owner: self.owner,
                    muted: None,
                    intent: agent_core::PublicationInputIntent::Retain(self.input),
                },
            )
            .await
            .map_err(|error| error.into_js())?;
        if self.closed.get() {
            return Err(operation_error_value(ffi::ErrorInfo::new(
                ffi::ErrorCode::Closed,
                "Pipe closed during input installation",
            )));
        }
        if let Some(state) = self.actor.pipe_state(self.input) {
            if let Ok(notification) = self.observer.prepare(state, false) {
                notification.notify();
            }
        }
        Ok(())
    }
}

#[wasm_bindgen]
impl BrowserPipe {
    pub fn state(&self) -> JsValue {
        pipe_state_value(&self.current_state())
    }

    pub fn updates(&self) -> Result<BrowserPipeUpdates, JsValue> {
        self.open_updates()
            .map(|updates| BrowserPipeUpdates { updates })
            .map_err(operation_error_value)
    }

    #[wasm_bindgen(unchecked_return_type = "Promise<void>")]
    pub fn close(&self) -> Result<js_sys::Promise, JsValue> {
        self.close_pipe().map_err(operation_error_value)
    }
}

impl BrowserPipe {
    pub(crate) fn current_state(&self) -> ffi::PipeState {
        let state = self.actor.pipe_state(self.input);
        let closed = state.is_none();
        if let Ok(notification) = self
            .observer
            .prepare(state.unwrap_or_else(|| self.observer.state()), closed)
        {
            notification.notify();
        }
        self.observer.state()
    }

    pub(crate) fn open_updates(&self) -> Result<observer::PipeUpdates, ffi::ErrorInfo> {
        self.current_state();
        self.observer.updates().map_err(|_| {
            ffi::ErrorInfo::new(
                ffi::ErrorCode::CapacityExceeded,
                "Pipe observer capacity exceeded",
            )
        })
    }

    pub(crate) fn close_pipe(&self) -> Result<js_sys::Promise, ffi::ErrorInfo> {
        if let Some(closing) = self.closing.borrow().as_ref() {
            return Ok(closing.clone());
        }
        let state = self
            .actor
            .pipe_state(self.input)
            .unwrap_or_else(|| self.observer.state());
        let detached = match self.actor.resource_now(ffi_resource_detach(self.input)) {
            Ok(agent_core::ResourceResult::PublicationInput { operation, .. }) => Some(operation),
            Ok(_) => None,
            Err(info) if info.code == ffi::ErrorCode::Closed => None,
            Err(info) => return Err(info),
        };
        self.closed.set(true);
        if let Some(inner) = self.inner.upgrade() {
            inner.fence_pipe_input(self.input, &self.slot);
        }
        let publication = self.inner.upgrade().and_then(|inner| {
            self.actor
                .publication_state(self.owner)
                .and_then(|state| inner.prepare_publication(self.owner, &state, false))
        });
        let pipe = self.observer.prepare(state, true).ok();
        let inner = self.inner.clone();
        let input = self.input;
        let actor = self.actor.clone();
        let slot = self.slot.clone();
        let cleanup = wasm_bindgen_futures::future_to_promise(async move {
            if let Some(inner) = inner.upgrade() {
                inner.forget_pipe(input);
                inner
                    .cleanup_pipe_input(input, &slot)
                    .await
                    .map_err(super::LocalOperationError::into_js)?;
            }
            if let Some(operation) = detached {
                let _ = actor.resource_now(agent_core::ResourceCommand::CompletePublicationInput {
                    operation,
                    result: Ok(()),
                });
            }
            Ok(JsValue::UNDEFINED)
        });
        *self.closing.borrow_mut() = Some(cleanup.clone());
        if let Some(notification) = publication {
            notification.notify();
        }
        if let Some(notification) = pipe {
            notification.notify();
        }
        Ok(cleanup)
    }
}

#[wasm_bindgen]
pub struct BrowserPipeUpdates {
    updates: observer::PipeUpdates,
}

#[wasm_bindgen]
impl BrowserPipeUpdates {
    pub async fn next(&self) -> Result<JsValue, JsValue> {
        let next = self.updates.next().map_err(|_| {
            operation_error_value(ffi::ErrorInfo::new(
                ffi::ErrorCode::Conflict,
                "An observation read is already pending",
            ))
        })?;
        Ok(next
            .await
            .as_ref()
            .map_or(JsValue::UNDEFINED, pipe_state_value))
    }

    pub fn close(&self) {
        self.updates.close();
    }
}

fn ffi_resource_detach(input: PublicationInputId) -> agent_core::ResourceCommand {
    agent_core::ResourceCommand::DetachPublicationInput { input }
}

fn pipe_state_value(state: &ffi::PipeState) -> JsValue {
    let value = Object::new();
    set(&value, "observationRevision", state.observation_revision);
    set(&value, "status", state.status as u32);
    set(
        &value,
        "error",
        state
            .error
            .as_ref()
            .map(error_value)
            .unwrap_or(JsValue::UNDEFINED),
    );
    value.into()
}

#[wasm_bindgen]
impl BrowserPublicationUpdates {
    pub async fn next(&self) -> Result<JsValue, JsValue> {
        let next = self.updates.next().map_err(|_| {
            operation_error_value(ffi::ErrorInfo::new(
                ffi::ErrorCode::Conflict,
                "An observation read is already pending",
            ))
        })?;
        Ok(next
            .await
            .as_ref()
            .map_or(JsValue::UNDEFINED, publication_state_value))
    }

    pub fn close(&self) {
        self.updates.close();
    }
}

fn publication_state_value(state: &ffi::PublicationState) -> JsValue {
    let value = Object::new();
    set(&value, "observationRevision", state.observation_revision);
    set(&value, "kind", state.kind as u32);
    set(&value, "trackId", state.track_id.clone());
    set(&value, "closed", state.closed);
    let configuration = Object::new();
    set(
        &configuration,
        "desiredRevision",
        state.configuration.desired_revision,
    );
    set(
        &configuration,
        "evaluatedRevision",
        state.configuration.evaluated_revision,
    );
    set(
        &configuration,
        "acceptedRevision",
        state.configuration.accepted_revision,
    );
    set(&configuration, "enabled", state.configuration.enabled);
    set(
        &configuration,
        "acceptedEnabled",
        state.configuration.accepted_enabled,
    );
    set(
        &configuration,
        "rejectedRevision",
        optional(state.configuration.rejected_revision),
    );
    set(
        &configuration,
        "error",
        state
            .configuration
            .error
            .as_ref()
            .map(error_value)
            .unwrap_or(JsValue::UNDEFINED),
    );
    set(&value, "configuration", configuration);
    let admission = Object::new();
    set(&admission, "status", state.admission.status as u32);
    set(
        &admission,
        "connectionGeneration",
        optional(state.admission.connection_generation),
    );
    set(
        &admission,
        "publicationId",
        state
            .admission
            .publication_id
            .clone()
            .map(JsValue::from)
            .unwrap_or(JsValue::UNDEFINED),
    );
    set(
        &admission,
        "evaluationPending",
        state.admission.evaluation_pending,
    );
    set(
        &admission,
        "error",
        state
            .admission
            .error
            .as_ref()
            .map(error_value)
            .unwrap_or(JsValue::UNDEFINED),
    );
    set(&admission, "execution", state.admission.execution as u32);
    set(
        &admission,
        "executionError",
        state
            .admission
            .execution_error
            .as_ref()
            .map(error_value)
            .unwrap_or(JsValue::UNDEFINED),
    );
    set(&admission, "demand", state.admission.demand as u32);
    set(&value, "admission", admission);
    set(&value, "inputAttached", state.input_attached);
    set(&value, "forwardingRequested", state.forwarding_requested);
    value.into()
}

pub(super) fn optional(value: Option<u64>) -> JsValue {
    value
        .map(|value| BigInt::from(value).into())
        .unwrap_or(JsValue::UNDEFINED)
}

pub(super) fn error_value(info: &ffi::ErrorInfo) -> JsValue {
    let value = Object::new();
    set(&value, "code", info.code as u32);
    set(&value, "message", info.message.clone());
    value.into()
}
