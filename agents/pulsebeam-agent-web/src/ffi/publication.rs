use super::{Agent, Arc, ErrorCode, ErrorInfo, WebAgentError};
use agent_core::{TrackSpec, ffi};

macro_rules! media_publication {
    ($track:ident, $publication:ident, $kind:ident, $options:ident) => {
        #[derive(uniffi::Object)]
        pub struct $track {
            #[cfg(target_arch = "wasm32")]
            agent: Arc<Agent>,
            spec: TrackSpec,
        }

        impl $track {
            pub(super) fn new(
                agent: Arc<Agent>,
                label: String,
            ) -> Result<Arc<Self>, WebAgentError> {
                let spec = TrackSpec::$kind { track_id: label };
                spec.validate().map_err(|_| {
                    ErrorInfo::new(ErrorCode::InvalidArgument, "Invalid publication label")
                })?;
                #[cfg(not(target_arch = "wasm32"))]
                let _ = agent;
                Ok(Arc::new(Self {
                    #[cfg(target_arch = "wasm32")]
                    agent,
                    spec,
                }))
            }
        }

        #[uniffi::export]
        impl $track {
            pub fn track_id(&self) -> String {
                self.spec.track_id().to_owned()
            }

            #[uniffi::method(default(options = None))]
            pub fn publish(
                &self,
                options: Option<ffi::$options>,
            ) -> Result<Arc<$publication>, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    let options =
                        agent_core::MediaPublishOptions::$kind(options.unwrap_or_default());
                    let (owner, observer) = claim(&self.agent, self.spec.clone(), options)?;
                    Ok(Arc::new($publication {
                        owner,
                        observer,
                        agent: self.agent.clone(),
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
        pub struct $publication {
            #[cfg(target_arch = "wasm32")]
            pub(super) agent: Arc<Agent>,
            #[cfg(target_arch = "wasm32")]
            pub(super) owner: u64,
            #[cfg(target_arch = "wasm32")]
            observer: std::rc::Rc<agent_core::observer::PublicationStateObserver>,
        }

        #[uniffi::export]
        impl $publication {
            pub fn configure(&self, options: ffi::PublicationConfig) -> Result<(), WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    self.agent
                        .runtime
                        .configure_publication_state(self.owner, options.enabled)?;
                    Ok(())
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let _ = options;
                    Err(super::browser_required())
                }
            }

            pub fn state(&self) -> Result<ffi::PublicationState, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    Ok(self.observer.state())
                }
                #[cfg(not(target_arch = "wasm32"))]
                Err(super::browser_required())
            }

            pub fn updates(&self) -> Result<Arc<PublicationUpdates>, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    let cursor = self.observer.updates().map_err(|_| {
                        ErrorInfo::new(ErrorCode::CapacityExceeded, "Observer capacity exceeded")
                    })?;
                    Ok(Arc::new(PublicationUpdates { cursor }))
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
    };
}

#[cfg(target_arch = "wasm32")]
fn claim(
    agent: &Agent,
    spec: TrackSpec,
    options: agent_core::MediaPublishOptions,
) -> Result<
    (
        u64,
        std::rc::Rc<agent_core::observer::PublicationStateObserver>,
    ),
    WebAgentError,
> {
    let runtime = &agent.runtime;
    let agent_core::ResourceResult::Publication { owner, .. } =
        runtime.claim_media_publication(spec, options)?
    else {
        return Err(ErrorInfo::new(ErrorCode::Internal, "Missing media publication owner").into());
    };
    let result = runtime.publication_state_observer(owner);
    if result.is_err() {
        let _ = runtime.release_media_owner(owner);
    }
    Ok((owner, result?))
}

media_publication!(
    LocalVideoTrack,
    VideoPublication,
    Video,
    VideoPublishOptions
);
media_publication!(LocalAudioTrack, AudioPublication, Audio, PublicationConfig);
state_cursor!(PublicationUpdates, PublicationState);
