use super::publication::error_value;
use super::*;
use crate::engine::ResourceNotification;
use agent_core::{ffi, observer};

pub(super) struct CatalogObserver {
    observer: observer::WatchStateObserver,
    projection: RefCell<Option<(ffi::WatchState, JsValue)>>,
}

impl CatalogObserver {
    fn new(state: ffi::WatchState) -> Self {
        Self {
            observer: observer::WatchStateObserver::new(state),
            projection: RefCell::new(None),
        }
    }

    fn prepare(&self, state: ffi::WatchState, closing: bool) -> Option<ResourceNotification> {
        let notification = self.observer.prepare(state, closing).ok()?;
        // Retire the old projection before delivery, even when no cursor reads again.
        self.value();
        Some(Box::new(move || {
            let _ = notification.notify();
        }))
    }

    fn value(&self) -> JsValue {
        let state = self.observer.state();
        let mut projection = self.projection.borrow_mut();
        if let Some((previous, value)) = projection.as_ref()
            && previous.observation_revision == state.observation_revision
        {
            return value.clone();
        }
        let page = state.page.as_ref().map_or(JsValue::UNDEFINED, |page| {
            if let Some((previous, value)) = projection.as_ref()
                && previous
                    .page
                    .as_ref()
                    .is_some_and(|previous| std::ptr::eq(previous.as_ref(), page.as_ref()))
            {
                return Reflect::get(value, &"page".into()).unwrap_or(JsValue::UNDEFINED);
            }
            page_value(page)
        });
        let value = Object::new();
        set(&value, "observationRevision", state.observation_revision);
        set(&value, "room", state.room.clone());
        set(&value, "limit", state.limit);
        set(&value, "cursor", bytes_value(&state.cursor));
        set(&value, "status", state.status as u32);
        set(&value, "page", page);
        set(
            &value,
            "error",
            state.error.as_ref().map_or(JsValue::UNDEFINED, |error| {
                let value: Object = error_value(error).unchecked_into();
                Object::freeze(&value).into()
            }),
        );
        let value: JsValue = Object::freeze(&value).into();
        *projection = Some((state, value.clone()));
        value
    }
}

#[wasm_bindgen]
pub struct BrowserCatalogWatch {
    owner: u64,
    runtime: Weak<RuntimeInner>,
    shared: Rc<CatalogObserver>,
    closed: Cell<bool>,
}

#[wasm_bindgen]
impl BrowserCatalogWatch {
    pub fn state(&self) -> JsValue {
        if let Some(runtime) = self.runtime.upgrade() {
            let notifications = runtime.prepare_current_catalogs();
            for notification in notifications {
                notification();
            }
        }
        self.shared.value()
    }

    pub fn updates(&self) -> Result<BrowserCatalogUpdates, JsValue> {
        let updates = self.shared.observer.updates().map_err(|_| {
            operation_error_value(ffi::ErrorInfo::new(
                ffi::ErrorCode::CapacityExceeded,
                "Watch observer capacity exceeded",
            ))
        })?;
        Ok(BrowserCatalogUpdates {
            shared: self.shared.clone(),
            updates,
        })
    }

    pub fn close(&self) {
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

impl Drop for BrowserCatalogWatch {
    fn drop(&mut self) {
        self.close();
    }
}

#[wasm_bindgen]
pub struct BrowserCatalogUpdates {
    shared: Rc<CatalogObserver>,
    updates: observer::WatchUpdates,
}

#[wasm_bindgen]
impl BrowserCatalogUpdates {
    pub async fn next(&self) -> Result<JsValue, JsValue> {
        let next = self.updates.next().map_err(|_| {
            operation_error_value(ffi::ErrorInfo::new(
                ffi::ErrorCode::Conflict,
                "A watch-state read is already pending",
            ))
        })?;
        Ok(if next.await.is_some() {
            self.shared.value()
        } else {
            JsValue::UNDEFINED
        })
    }
    pub fn close(&self) {
        self.updates.close();
    }
}

#[wasm_bindgen]
impl BrowserRuntime {
    pub fn catalog_watch(
        &self,
        room: String,
        limit: u32,
        cursor: Vec<u8>,
    ) -> Result<BrowserCatalogWatch, JsValue> {
        if self.inner.is_closed() {
            return Err(operation_error_value(ffi::ErrorInfo::new(
                ffi::ErrorCode::Closed,
                "Watch runtime is closed",
            )));
        }
        let (owner, state) = self
            .inner
            .actor
            .borrow()
            .as_ref()
            .ok_or_else(|| {
                operation_error_value(ffi::ErrorInfo::new(
                    ffi::ErrorCode::Closed,
                    "Actor is not running",
                ))
            })?
            .open_catalog_watch(room, ffi::WatchConfig { limit, cursor })
            .map_err(operation_error_value)?;
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

fn bytes_value(bytes: &[u8]) -> JsValue {
    let array = Array::new();
    for byte in bytes {
        array.push(&JsValue::from(*byte));
    }
    Object::freeze(&array).into()
}

fn page_value(page: &ffi::CatalogPage) -> JsValue {
    let value = Object::new();
    set(&value, "revision", page.revision());
    let participants = Array::new();
    for participant in page.participants() {
        let value = Object::new();
        set(&value, "id", participant.id);
        let publications = Array::new();
        for publication in participant.publications {
            let value = Object::new();
            set(&value, "id", publication.id);
            set(&value, "label", publication.label);
            let kind = Object::new();
            match publication.kind {
                ffi::CatalogPublicationKind::Audio => set(&kind, "tag", "Audio"),
                ffi::CatalogPublicationKind::Video => set(&kind, "tag", "Video"),
                ffi::CatalogPublicationKind::Data { delivery } => {
                    set(&kind, "tag", "Data");
                    let inner = Object::new();
                    set(&inner, "delivery", delivery as u32);
                    set(&kind, "inner", Object::freeze(&inner));
                }
            }
            set(&value, "kind", Object::freeze(&kind));
            publications.push(&Object::freeze(&value));
        }
        set(&value, "publications", Object::freeze(&publications));
        participants.push(&Object::freeze(&value));
    }
    set(&value, "participants", Object::freeze(&participants));
    set(&value, "hasMore", page.has_more());
    set(&value, "nextCursor", bytes_value(&page.next_cursor()));
    set(
        &value,
        "cursorExpiresAtUnixSeconds",
        page.cursor_expires_at_unix_seconds(),
    );
    Object::freeze(&value).into()
}
