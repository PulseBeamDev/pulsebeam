use super::{Agent, Arc, ErrorCode, ErrorInfo, WebAgentError, video::Participant};
use agent_core::{TopicMode, TrackSpec, ffi};

fn ordered_track(label: String) -> Result<TrackSpec, WebAgentError> {
    let spec = TrackSpec::Data {
        track_id: label,
        delivery: TopicMode::Ordered,
    };
    spec.validate()
        .map_err(|_| ErrorInfo::new(ErrorCode::InvalidArgument, "Invalid data track label"))?;
    Ok(spec)
}

#[derive(uniffi::Object)]
pub struct LocalDataTrack {
    #[cfg(target_arch = "wasm32")]
    agent: Arc<Agent>,
    #[cfg(target_arch = "wasm32")]
    spec: TrackSpec,
}

#[uniffi::export]
impl Agent {
    pub fn data(self: Arc<Self>, label: String) -> Result<Arc<LocalDataTrack>, WebAgentError> {
        let spec = ordered_track(label)?;
        #[cfg(not(target_arch = "wasm32"))]
        let _ = spec;
        Ok(Arc::new(LocalDataTrack {
            #[cfg(target_arch = "wasm32")]
            agent: self,
            #[cfg(target_arch = "wasm32")]
            spec,
        }))
    }
}

#[uniffi::export]
impl LocalDataTrack {
    pub fn publish(&self) -> Result<Arc<DataPublication>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let (owner, observer) = self
                .agent
                .runtime
                .claim_data_publication(self.spec.clone())?;
            Ok(Arc::new(DataPublication {
                agent: self.agent.clone(),
                owner,
                observer,
            }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(super::browser_required())
    }
}

#[derive(uniffi::Object)]
pub struct RemoteDataTrack {
    #[cfg(target_arch = "wasm32")]
    agent: Arc<Agent>,
    #[cfg(target_arch = "wasm32")]
    spec: TrackSpec,
    #[cfg(target_arch = "wasm32")]
    selector: agent_core::TrackSelector,
}

#[uniffi::export]
impl Participant {
    pub fn data(&self, label: String) -> Result<Arc<RemoteDataTrack>, WebAgentError> {
        let spec = ordered_track(label.clone())?;
        let selector = self.selector(label)?;
        #[cfg(not(target_arch = "wasm32"))]
        let _ = (spec, selector);
        Ok(Arc::new(RemoteDataTrack {
            #[cfg(target_arch = "wasm32")]
            agent: self.agent.clone(),
            #[cfg(target_arch = "wasm32")]
            spec,
            #[cfg(target_arch = "wasm32")]
            selector,
        }))
    }
}

#[uniffi::export]
impl RemoteDataTrack {
    pub fn subscribe(&self) -> Result<Arc<DataSubscription>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let (owner, observer) = self
                .agent
                .runtime
                .claim_data_subscription(self.spec.clone(), self.selector.clone())?;
            Ok(Arc::new(DataSubscription {
                agent: self.agent.clone(),
                owner,
                observer,
            }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(super::browser_required())
    }
}

macro_rules! data_owner {
    ($name:ident, $state:ident, $observer:ident, $cursor:ident) => {
        #[derive(uniffi::Object)]
        pub struct $name {
            #[cfg(target_arch = "wasm32")]
            agent: Arc<Agent>,
            #[cfg(target_arch = "wasm32")]
            owner: u64,
            #[cfg(target_arch = "wasm32")]
            observer: std::rc::Rc<agent_core::observer::$observer>,
        }

        #[uniffi::export]
        impl $name {
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

data_owner!(
    DataPublication,
    DataPublicationState,
    DataPublicationStateObserver,
    DataPublicationUpdates
);
data_owner!(
    DataSubscription,
    DataSubscriptionState,
    DataSubscriptionStateObserver,
    DataSubscriptionUpdates
);
