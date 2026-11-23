use super::{Agent, Arc, ErrorCode, ErrorInfo, WebAgentError};
use agent_core::{ffi, observer};

fn invalid_address() -> ErrorInfo {
    ErrorInfo::new(ErrorCode::InvalidArgument, "Invalid video source address")
}

#[derive(uniffi::Object)]
pub struct RoomRef {
    agent: Arc<Agent>,
    room: String,
}

impl RoomRef {
    pub(super) fn new(agent: Arc<Agent>, room: String) -> Result<Self, WebAgentError> {
        agent_core::validate_room_name(&room).map_err(|_| invalid_address())?;
        Ok(Self { agent, room })
    }
}

#[uniffi::export]
impl RoomRef {
    pub fn participant(&self, id: String) -> Result<Arc<Participant>, WebAgentError> {
        agent_core::validate_participant_id(&id).map_err(|_| invalid_address())?;
        Ok(Arc::new(Participant {
            agent: self.agent.clone(),
            room: self.room.clone(),
            id,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct Participant {
    agent: Arc<Agent>,
    room: String,
    id: String,
}

#[uniffi::export]
impl Participant {
    pub fn video(&self, label: String) -> Result<Arc<RemoteVideoTrack>, WebAgentError> {
        let selector = agent_core::TrackSelector {
            room: self.room.clone(),
            participant_id: self.id.clone(),
            track_id: label,
        };
        selector.validate().map_err(|_| invalid_address())?;
        Ok(Arc::new(RemoteVideoTrack {
            #[cfg(target_arch = "wasm32")]
            agent: self.agent.clone(),
            #[cfg(target_arch = "wasm32")]
            selector,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct RemoteVideoTrack {
    #[cfg(target_arch = "wasm32")]
    agent: Arc<Agent>,
    #[cfg(target_arch = "wasm32")]
    selector: agent_core::TrackSelector,
}

#[uniffi::export]
impl RemoteVideoTrack {
    #[uniffi::method(default(options = None))]
    pub fn subscribe(
        &self,
        options: Option<ffi::VideoObservation>,
    ) -> Result<Arc<VideoSubscription>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let configuration = options.unwrap_or_else(|| ffi::VideoObservation {
                visible: true,
                ..Default::default()
            });
            let (owner, observer) = self
                .agent
                .runtime
                .claim_video(self.selector.clone(), configuration.into())?;
            Ok(Arc::new(VideoSubscription {
                agent: self.agent.clone(),
                owner,
                observer,
            }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = options;
            Err(super::browser_required())
        }
    }
}

#[derive(uniffi::Object)]
pub struct VideoSubscription {
    #[cfg(target_arch = "wasm32")]
    agent: Arc<Agent>,
    #[cfg(target_arch = "wasm32")]
    owner: u64,
    #[cfg(target_arch = "wasm32")]
    observer: std::rc::Rc<observer::VideoSubscriptionStateObserver>,
}

#[uniffi::export]
impl VideoSubscription {
    pub fn state(&self) -> Result<ffi::VideoSubscriptionState, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            Ok(self.observer.state())
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(super::browser_required())
    }

    pub fn updates(&self) -> Result<Arc<VideoSubscriptionUpdates>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let cursor = self.observer.updates().map_err(|_| {
                ErrorInfo::new(ErrorCode::CapacityExceeded, "Observer capacity exceeded")
            })?;
            Ok(Arc::new(VideoSubscriptionUpdates { cursor }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(super::browser_required())
    }

    pub async fn playback_output(&self) -> Result<Arc<WebVideoOutput>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let observation = self
                .agent
                .runtime
                .observe_video(self.owner)
                .await
                .map_err(|_| {
                    if self.observer.state().closed {
                        ErrorInfo::new(ErrorCode::Closed, "Video subscription is closed")
                    } else {
                        ErrorInfo::new(ErrorCode::NotReady, "Video output is unavailable")
                    }
                })?;
            WebVideoOutput::new(observation).map(Arc::new)
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(super::browser_required())
    }

    pub async fn close(&self) -> Result<(), WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        if !self.observer.state().closed {
            self.agent.runtime.close_video(self.owner)?;
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mod playback {
    use super::{ErrorCode, ErrorInfo, WebAgentError, WebVideoOutput};
    use crate::browser::media_observation::BrowserMediaObservation;
    use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
    use web_sys::MediaStreamTrack;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(catch, js_namespace = ["globalThis", "__pulsebeamMediaRegistry"], js_name = retain)]
        fn retain(track: &MediaStreamTrack, kind: &str) -> Result<u64, JsValue>;
        #[wasm_bindgen(catch, js_namespace = ["globalThis", "__pulsebeamMediaRegistry"], js_name = release)]
        fn release(handle: u64, kind: &str) -> Result<bool, JsValue>;
    }

    impl WebVideoOutput {
        pub(super) fn new(observation: BrowserMediaObservation) -> Result<Self, WebAgentError> {
            let track = observation
                .track()
                .ok_or_else(|| ErrorInfo::new(ErrorCode::Closed, "Video output is closed"))?;
            let handle = retain(&track, "track").map_err(|_| {
                ErrorInfo::new(
                    ErrorCode::Internal,
                    "Could not register browser video output",
                )
            })?;
            Ok(Self {
                observation,
                handle: std::cell::Cell::new(Some(handle)),
            })
        }

        pub(super) fn release_handle(&self) {
            if let Some(handle) = self.handle.take() {
                let _ = release(handle, "track");
            }
        }
    }

    impl Drop for WebVideoOutput {
        fn drop(&mut self) {
            self.release_handle();
        }
    }
}

#[derive(uniffi::Object)]
pub struct WebVideoOutput {
    #[cfg(target_arch = "wasm32")]
    observation: crate::browser::media_observation::BrowserMediaObservation,
    #[cfg(target_arch = "wasm32")]
    handle: std::cell::Cell<Option<u64>>,
}

#[uniffi::export]
impl WebVideoOutput {
    pub fn track(&self) -> Option<super::WebMediaTrack> {
        #[cfg(target_arch = "wasm32")]
        {
            (!self.observation.is_closed())
                .then(|| self.handle.get())
                .flatten()
                .map(super::WebMediaTrack)
        }
        #[cfg(not(target_arch = "wasm32"))]
        None
    }

    pub async fn close(&self) {
        #[cfg(target_arch = "wasm32")]
        {
            self.release_handle();
            self.observation.close_local().await;
        }
    }
}

#[derive(uniffi::Object)]
pub struct VideoSubscriptionUpdates {
    cursor: observer::VideoSubscriptionUpdates,
}

#[uniffi::export]
impl VideoSubscriptionUpdates {
    pub async fn next(&self) -> Result<Option<ffi::VideoSubscriptionState>, WebAgentError> {
        let next = self.cursor.next().map_err(|_| {
            ErrorInfo::new(
                ErrorCode::Conflict,
                "An observation read is already pending",
            )
        })?;
        Ok(next.await)
    }

    pub fn close(&self) {
        self.cursor.close();
    }
}
