use super::{Agent, Arc, ErrorCode, ErrorInfo, SelectorSubscription, WebAgentError};
use agent_core::ffi;

fn invalid_address() -> ErrorInfo {
    ErrorInfo::new(ErrorCode::InvalidArgument, "Invalid remote source address")
}

#[derive(uniffi::Object)]
pub struct RoomRef {
    pub(super) agent: Arc<Agent>,
    pub(super) room: String,
}

impl RoomRef {
    pub(super) fn new(agent: Arc<Agent>, room: String) -> Result<Self, WebAgentError> {
        agent_core::validate_room_name(&room).map_err(|_| invalid_address())?;
        Ok(Self { agent, room })
    }
}

#[uniffi::export]
impl RoomRef {
    pub fn name(&self) -> String {
        self.room.clone()
    }

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
    pub(super) agent: Arc<Agent>,
    room: String,
    id: String,
}

#[uniffi::export]
impl Participant {
    pub fn id(&self) -> String {
        self.id.clone()
    }

    pub fn video(&self, label: String) -> Result<Arc<RemoteVideoTrack>, WebAgentError> {
        RemoteVideoTrack::new(self, label)
    }

    pub fn audio(&self, label: String) -> Result<Arc<RemoteAudioTrack>, WebAgentError> {
        RemoteAudioTrack::new(self, label)
    }
}

impl Participant {
    pub(super) fn selector(
        &self,
        label: String,
    ) -> Result<agent_core::TrackSelector, WebAgentError> {
        let selector = agent_core::TrackSelector {
            room: self.room.clone(),
            participant_id: self.id.clone(),
            track_id: label,
        };
        selector.validate().map_err(|_| invalid_address())?;
        Ok(selector)
    }
}

macro_rules! subscription {
    ($track:ident, $subscription:ident, $state:ident, $observer:ident, $cursor:ident,
     $claim:ident $(, $options:ident, $configure:ident, $configuration:expr, $arg:ident)?) => {
        #[derive(uniffi::Object)]
        pub struct $track {
            #[cfg(target_arch = "wasm32")]
            agent: Arc<Agent>,
            selector: agent_core::TrackSelector,
        }

        impl $track {
            fn new(participant: &Participant, label: String) -> Result<Arc<Self>, WebAgentError> {
                let selector = participant.selector(label)?;
                Ok(Arc::new(Self {
                    #[cfg(target_arch = "wasm32")]
                    agent: participant.agent.clone(),
                    selector,
                }))
            }
        }

        #[uniffi::export]
        impl $track {
            pub fn selector(&self) -> ffi::TrackSelector {
                self.selector.clone().into()
            }

            $(#[uniffi::method(default($arg = None))])?
            pub fn subscribe(&self $(, $arg: Option<ffi::$options>)?) -> Result<Arc<$subscription>, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    let (owner, observer) = self
                        .agent
                        .runtime
                        .$claim(self.selector.clone() $(, ($configuration)($arg))?)?;
                    Ok(Arc::new($subscription {
                        agent: self.agent.clone(),
                        owner,
                        observer,
                    }))
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    $(let _ = $arg;)?
                    Err(super::browser_required())
                }
            }
        }

        #[derive(uniffi::Object)]
        pub struct $subscription {
            #[cfg(target_arch = "wasm32")]
            agent: Arc<Agent>,
            #[cfg(target_arch = "wasm32")]
            owner: u64,
            #[cfg(target_arch = "wasm32")]
            observer: std::rc::Rc<agent_core::observer::$observer>,
        }

        #[uniffi::export]
        impl $subscription {
            $(
            #[uniffi::method(default($arg = None))]
            pub fn configure(&self, $arg: Option<ffi::$options>) -> Result<(), WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    self.agent
                        .runtime
                        .$configure(self.owner, ($configuration)($arg))?;
                    Ok(())
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let _ = $arg;
                    Err(super::browser_required())
                }
            }
            )?

            pub fn state(&self) -> Result<ffi::$state, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    Ok(self.observer.state())
                }
                #[cfg(not(target_arch = "wasm32"))]
                Err(super::browser_required())
            }

            pub fn updates(&self) -> Result<Arc<$cursor>, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    let cursor = self.observer.updates().map_err(|_| {
                        ErrorInfo::new(ErrorCode::CapacityExceeded, "Observer capacity exceeded")
                    })?;
                    Ok(Arc::new($cursor { cursor }))
                }
                #[cfg(not(target_arch = "wasm32"))]
                Err(super::browser_required())
            }

            pub async fn close(&self) -> Result<(), WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                if !self.observer.state().closed {
                    self.agent.runtime.release_media_owner(self.owner)?;
                }
                Ok(())
            }
        }

        state_cursor!($cursor, $state);
    };
}

subscription!(
    RemoteVideoTrack,
    VideoSubscription,
    VideoSubscriptionState,
    VideoSubscriptionStateObserver,
    VideoSubscriptionUpdates,
    claim_video,
    VideoObservation,
    configure_owned_video,
    |options: Option<ffi::VideoObservation>| options
        .unwrap_or_else(|| ffi::VideoObservation {
            visible: true,
            ..Default::default()
        })
        .into(),
    options
);
subscription!(
    RemoteAudioTrack,
    AudioSubscription,
    AudioSubscriptionState,
    AudioSubscriptionStateObserver,
    AudioSubscriptionUpdates,
    claim_audio
);

#[cfg(target_arch = "wasm32")]
mod playback {
    use super::{ErrorCode, ErrorInfo, WebAgentError};
    use crate::{
        browser::media_observation::BrowserMediaObservation, ffi::retained_track::RetainedTrack,
    };
    use std::cell::RefCell;

    pub(super) struct Output {
        observation: BrowserMediaObservation,
        handle: RefCell<Option<RetainedTrack>>,
    }

    impl Output {
        pub(super) fn new(observation: BrowserMediaObservation) -> Result<Self, WebAgentError> {
            let track = observation
                .track()
                .ok_or_else(|| ErrorInfo::new(ErrorCode::Closed, "Playback output is closed"))?;
            let handle = RetainedTrack::new(&track)?;
            Ok(Self {
                observation,
                handle: RefCell::new(Some(handle)),
            })
        }

        pub(super) fn track(&self) -> Option<crate::ffi::WebMediaTrack> {
            if self.observation.is_closed() {
                self.handle.take();
            }
            self.handle.borrow().as_ref().map(RetainedTrack::handle)
        }

        pub(super) async fn close(&self) {
            self.handle.take();
            self.observation.close_local().await;
        }
    }
}

macro_rules! playback_output {
    ($subscription:ident, $output:ident) => {
        #[uniffi::export]
        impl $subscription {
            pub async fn playback_output(&self) -> Result<Arc<$output>, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    let observation = self
                        .agent
                        .runtime
                        .observe_exact_media(self.owner)
                        .await
                        .map_err(|_| {
                            if self.observer.state().closed {
                                ErrorInfo::new(ErrorCode::Closed, "Media subscription is closed")
                            } else {
                                ErrorInfo::new(
                                    ErrorCode::NotReady,
                                    "Playback output is unavailable",
                                )
                            }
                        })?;
                    Ok(Arc::new($output {
                        output: playback::Output::new(observation)?,
                    }))
                }
                #[cfg(not(target_arch = "wasm32"))]
                Err(super::browser_required())
            }
        }

        media_output!($output);
    };
}

macro_rules! media_output {
    ($output:ident) => {
        #[derive(uniffi::Object)]
        pub struct $output {
            #[cfg(target_arch = "wasm32")]
            output: playback::Output,
        }

        #[uniffi::export]
        impl $output {
            pub fn track(&self) -> Option<super::WebMediaTrack> {
                #[cfg(target_arch = "wasm32")]
                {
                    self.output.track()
                }
                #[cfg(not(target_arch = "wasm32"))]
                None
            }

            pub async fn close(&self) {
                #[cfg(target_arch = "wasm32")]
                self.output.close().await;
            }
        }
    };
}

playback_output!(VideoSubscription, WebVideoOutput);
playback_output!(AudioSubscription, WebAudioOutput);
media_output!(WebSelectorOutput);

#[uniffi::export]
impl SelectorSubscription {
    pub async fn playback_output(
        &self,
        source: agent_core::SelectorMatch,
    ) -> Result<Arc<WebSelectorOutput>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            if source.kind == agent_core::SelectorTrackKind::Data {
                return Err(ErrorInfo::new(
                    ErrorCode::Unsupported,
                    "Data matches have no playback output",
                )
                .into());
            }
            let observation = self
                .agent
                .runtime
                .observe_selector_media(self.owner, source)
                .await
                .map_err(|_| {
                    if self.state().is_none() {
                        ErrorInfo::new(ErrorCode::Closed, "Subscription selector is closed")
                    } else {
                        ErrorInfo::new(
                            ErrorCode::NotReady,
                            "Selected playback output is unavailable",
                        )
                    }
                })?;
            Ok(Arc::new(WebSelectorOutput {
                output: playback::Output::new(observation)?,
            }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = source;
            Err(super::browser_required())
        }
    }
}
