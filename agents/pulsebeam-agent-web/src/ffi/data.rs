use super::{Agent, Arc, ErrorCode, ErrorInfo, WebAgentError, video::Participant};
use agent_core::{TrackSpec, ffi};

fn track_spec(label: String, delivery: ffi::DataDelivery) -> TrackSpec {
    TrackSpec::Data {
        track_id: label,
        delivery: delivery.into(),
    }
}

#[derive(uniffi::Object)]
pub struct LocalDataTrack {
    #[cfg(target_arch = "wasm32")]
    agent: Arc<Agent>,
    label: String,
}

#[uniffi::export]
impl Agent {
    pub fn data(self: Arc<Self>, label: String) -> Result<Arc<LocalDataTrack>, WebAgentError> {
        track_spec(label.clone(), ffi::DataDelivery::default())
            .validate()
            .map_err(|_| ErrorInfo::new(ErrorCode::InvalidArgument, "Invalid data track label"))?;
        Ok(Arc::new(LocalDataTrack {
            #[cfg(target_arch = "wasm32")]
            agent: self,
            label,
        }))
    }
}

#[uniffi::export]
impl LocalDataTrack {
    pub fn track_id(&self) -> String {
        self.label.clone()
    }

    #[uniffi::method(default(options = None))]
    pub fn publish(
        &self,
        options: Option<ffi::DataPublishOptions>,
    ) -> Result<Arc<DataPublication>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let (owner, observer) = self.agent.runtime.claim_data_publication(track_spec(
                self.label.clone(),
                options.and_then(|value| value.delivery).unwrap_or_default(),
            ))?;
            Ok(Arc::new(DataPublication {
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
pub struct RemoteDataTrack {
    #[cfg(target_arch = "wasm32")]
    agent: Arc<Agent>,
    selector: agent_core::TrackSelector,
}

#[uniffi::export]
impl Participant {
    pub fn data(&self, label: String) -> Result<Arc<RemoteDataTrack>, WebAgentError> {
        let selector = self.selector(label)?;
        Ok(Arc::new(RemoteDataTrack {
            #[cfg(target_arch = "wasm32")]
            agent: self.agent.clone(),
            selector,
        }))
    }
}

#[uniffi::export]
impl RemoteDataTrack {
    pub fn selector(&self) -> ffi::TrackSelector {
        self.selector.clone().into()
    }

    #[uniffi::method(default(options = None))]
    pub fn subscribe(
        &self,
        options: Option<ffi::DataSubscribeOptions>,
    ) -> Result<Arc<DataSubscription>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let (owner, observer) = self.agent.runtime.claim_data_subscription(
                track_spec(
                    self.selector.track_id.clone(),
                    options.and_then(|value| value.delivery).unwrap_or_default(),
                ),
                self.selector.clone(),
            )?;
            Ok(Arc::new(DataSubscription {
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

#[uniffi::export]
impl DataPublication {
    #[uniffi::method(default(options = None))]
    pub async fn send(
        &self,
        bytes: Vec<u8>,
        options: Option<ffi::DataSendOptions>,
    ) -> Result<(), WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            if self.observer.state().closed {
                return Err(ErrorInfo::new(ErrorCode::Closed, "Data publication is closed").into());
            }
            self.agent
                .runtime
                .send_exact_data(self.owner, bytes, options)
                .await?;
            Ok(())
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (bytes, options);
            Err(super::browser_required())
        }
    }
}

#[uniffi::export]
impl DataSubscription {
    pub async fn next(&self) -> Result<Option<agent_core::DataEvent>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            Ok(self
                .agent
                .runtime
                .next_exact_data(self.owner, &self.observer)
                .await?)
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
