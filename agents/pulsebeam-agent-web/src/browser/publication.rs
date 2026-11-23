use std::rc::Rc;

use crate::engine::ActorHandle;

use wasm_bindgen::JsValue;

use super::operation_error_value;
use agent_core::{PublicationInputId, ffi, observer};

pub(crate) struct BrowserPipe {
    pub(super) actor: ActorHandle,
    pub(super) input: PublicationInputId,
    pub(super) observer: Rc<observer::PipeStateObserver>,
    pub(super) closed: std::cell::Cell<bool>,
    pub(super) owner: u64,
    pub(super) slot: String,
    pub(super) inner: std::rc::Weak<super::RuntimeInner>,
    pub(super) closing: std::cell::RefCell<Option<js_sys::Promise>>,
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

fn ffi_resource_detach(input: PublicationInputId) -> agent_core::ResourceCommand {
    agent_core::ResourceCommand::DetachPublicationInput { input }
}
