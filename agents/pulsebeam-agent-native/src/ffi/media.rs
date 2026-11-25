use super::*;

macro_rules! source {
    ($name:ident) => {
        #[derive(uniffi::Object)]
        pub struct $name {
            runtime: crate::ProducerInput,
        }

        #[uniffi::export(async_runtime = "tokio")]
        impl $name {
            pub fn is_closed(&self) -> bool {
                self.runtime.is_closed()
            }
            pub async fn submit(&self, frame: InputFrame) -> Result<(), NativeError> {
                self.runtime.submit(frame).await.map_err(Into::into)
            }
            pub async fn capture_time_us(&self) -> Result<i64, NativeError> {
                self.runtime.capture_time_us().await.map_err(Into::into)
            }
            pub async fn close(&self) -> Result<(), NativeError> {
                self.runtime.close().await.map_err(Into::into)
            }
        }
    };
}

source!(VideoSource);
source!(AudioSource);

#[uniffi::export(async_runtime = "tokio")]
impl VideoSource {
    pub fn pipe_to(&self, publication: &VideoPublication) -> Result<Arc<Pipe>, NativeError> {
        Ok(Arc::new(Pipe {
            runtime: self.runtime.pipe_to(&publication.runtime)?,
        }))
    }

    pub async fn set_limits(
        &self,
        limits: Vec<EncodingLimits>,
    ) -> Result<Vec<EncodingState>, NativeError> {
        self.runtime.set_limits(limits).await.map_err(Into::into)
    }
    pub fn feedback(&self) -> Arc<FeedbackStream> {
        Arc::new(FeedbackStream {
            updates: self.runtime.feedback(),
        })
    }
}

macro_rules! publication {
    ($name:ident) => {
        #[derive(uniffi::Object)]
        pub struct $name {
            runtime: RuntimePublication,
        }

        #[uniffi::export(async_runtime = "tokio")]
        impl $name {
            pub fn is_closed(&self) -> bool {
                self.runtime.is_closed()
            }
            pub fn state(&self) -> core_ffi::PublicationState {
                self.runtime.state()
            }
            pub fn updates(&self) -> Result<Arc<PublicationUpdates>, NativeError> {
                let updates = self.runtime.updates().map_err(|_| NativeError::Operation {
                    info: core_ffi::ErrorInfo::new(
                        core_ffi::ErrorCode::CapacityExceeded,
                        "Publication observer capacity exceeded",
                    ),
                })?;
                Ok(Arc::new(PublicationUpdates { runtime: updates }))
            }
            pub async fn configure(
                &self,
                configuration: core_ffi::PublicationConfig,
            ) -> Result<(), NativeError> {
                self.runtime
                    .configure(configuration)
                    .await
                    .map_err(Into::into)
            }
            pub async fn close(&self) -> Result<(), NativeError> {
                self.runtime.close().await.map_err(Into::into)
            }
        }

        #[cfg(feature = "sim")]
        impl $name {
            pub async fn ready(&self) -> Result<(), NativeError> {
                self.runtime.ready().await.map_err(Into::into)
            }
        }
    };
}

publication!(VideoPublication);
publication!(AudioPublication);

#[uniffi::export]
impl AudioSource {
    pub fn pipe_to(&self, publication: &AudioPublication) -> Result<Arc<Pipe>, NativeError> {
        Ok(Arc::new(Pipe {
            runtime: self.runtime.pipe_to(&publication.runtime)?,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct Pipe {
    runtime: crate::Pipe,
}

#[uniffi::export(async_runtime = "tokio")]
impl Pipe {
    pub fn state(&self) -> core_ffi::PipeState {
        self.runtime.state()
    }

    pub fn updates(&self) -> Result<Arc<PipeUpdates>, NativeError> {
        let updates = self.runtime.updates().map_err(|_| NativeError::Operation {
            info: core_ffi::ErrorInfo::new(
                core_ffi::ErrorCode::CapacityExceeded,
                "Pipe observer capacity exceeded",
            ),
        })?;
        Ok(Arc::new(PipeUpdates { runtime: updates }))
    }

    pub async fn close(&self) -> Result<(), NativeError> {
        self.runtime.close().await.map_err(Into::into)
    }
}

#[derive(uniffi::Object)]
pub struct PipeUpdates {
    runtime: agent_core::observer::PipeUpdates,
}

#[uniffi::export(async_runtime = "tokio")]
impl PipeUpdates {
    pub async fn next(&self) -> Result<Option<core_ffi::PipeState>, NativeError> {
        Ok(self
            .runtime
            .next()
            .map_err(|_| NativeError::Operation {
                info: core_ffi::ErrorInfo::new(
                    core_ffi::ErrorCode::Conflict,
                    "An observation read is already pending",
                ),
            })?
            .await)
    }

    pub fn close(&self) {
        self.runtime.close();
    }
}

#[derive(uniffi::Object)]
pub struct PublicationUpdates {
    runtime: crate::PublicationUpdates,
}

#[uniffi::export(async_runtime = "tokio")]
impl PublicationUpdates {
    pub async fn next(&self) -> Result<Option<core_ffi::PublicationState>, NativeError> {
        self.runtime
            .next()
            .await
            .map_err(|_| NativeError::Operation {
                info: core_ffi::ErrorInfo::new(
                    core_ffi::ErrorCode::Conflict,
                    "An observation read is already pending",
                ),
            })
    }
    pub fn close(&self) {
        self.runtime.close();
    }
}

#[derive(uniffi::Object)]
pub struct LocalVideoTrack {
    runtime: crate::LocalVideoTrack,
}

#[derive(uniffi::Object)]
pub struct LocalAudioTrack {
    runtime: crate::LocalAudioTrack,
}

#[uniffi::export]
impl LocalVideoTrack {
    #[uniffi::method(default(options = None))]
    pub fn publish(
        &self,
        options: Option<core_ffi::VideoPublishOptions>,
    ) -> Result<Arc<VideoPublication>, NativeError> {
        Ok(Arc::new(VideoPublication {
            runtime: self.runtime.publish(options.unwrap_or_default())?,
        }))
    }
}

#[uniffi::export]
impl LocalAudioTrack {
    #[uniffi::method(default(options = None))]
    pub fn publish(
        &self,
        options: Option<core_ffi::PublicationConfig>,
    ) -> Result<Arc<AudioPublication>, NativeError> {
        Ok(Arc::new(AudioPublication {
            runtime: self.runtime.publish(options.unwrap_or_default())?,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct RemoteVideoTrack {
    runtime: crate::RemoteVideoTrack,
}

#[uniffi::export]
impl RemoteVideoTrack {
    #[uniffi::method(default(configuration = None))]
    pub fn subscribe(
        &self,
        configuration: Option<core_ffi::VideoObservation>,
    ) -> Result<Arc<VideoSubscription>, NativeError> {
        let configuration = configuration.unwrap_or_else(|| core_ffi::VideoObservation {
            visible: true,
            ..Default::default()
        });
        let runtime = self.runtime.subscribe(configuration.into())?;
        let observer = runtime.video_observer()?;
        Ok(Arc::new(VideoSubscription { runtime, observer }))
    }
}

#[derive(uniffi::Object)]
pub struct RemoteAudioTrack {
    runtime: crate::RemoteAudioTrack,
}

#[uniffi::export]
impl RemoteAudioTrack {
    pub fn subscribe(&self) -> Result<Arc<AudioSubscription>, NativeError> {
        let runtime = self.runtime.subscribe()?;
        let observer = runtime.audio_observer()?;
        Ok(Arc::new(AudioSubscription { runtime, observer }))
    }
}

macro_rules! subscription {
    ($name:ident, $state:ident, $observer:ident, $updates:ident) => {
        #[derive(uniffi::Object)]
        pub struct $name {
            runtime: RuntimeSubscription,
            observer: Arc<agent_core::observer::$observer>,
        }

        #[uniffi::export(async_runtime = "tokio")]
        impl $name {
            pub fn state(&self) -> core_ffi::$state {
                self.runtime.refresh_state();
                self.observer.state()
            }
            pub fn updates(&self) -> Result<Arc<$updates>, NativeError> {
                self.state();
                Ok(Arc::new($updates {
                    runtime: self
                        .observer
                        .updates()
                        .map_err(resource_state::observer_limit)?,
                }))
            }
            pub fn is_closed(&self) -> bool {
                self.runtime.is_closed()
            }
            pub async fn observe(
                &self,
                mode: ReceiveMode,
            ) -> Result<Arc<RemoteMediaReceiver>, NativeError> {
                Ok(Arc::new(RemoteMediaReceiver {
                    runtime: self.runtime.observe(mode).await?,
                }))
            }
            pub async fn close(&self) -> Result<(), NativeError> {
                self.runtime.close().await.map_err(Into::into)
            }
        }
    };
}

subscription!(
    VideoSubscription,
    VideoSubscriptionState,
    VideoSubscriptionStateObserver,
    VideoSubscriptionUpdates
);
subscription!(
    AudioSubscription,
    AudioSubscriptionState,
    AudioSubscriptionStateObserver,
    AudioSubscriptionUpdates
);

#[uniffi::export(async_runtime = "tokio")]
impl VideoSubscription {
    pub fn frames(&self) -> Result<Arc<VideoFrames>, NativeError> {
        Ok(Arc::new(VideoFrames::new(
            self.runtime.clone(),
            self.observer.clone(),
        )?))
    }

    pub async fn configure(
        &self,
        configuration: core_ffi::VideoObservation,
    ) -> Result<(), NativeError> {
        self.runtime
            .configure_video(configuration.into())
            .await
            .map_err(Into::into)
    }
}

#[uniffi::export]
impl AudioSubscription {
    pub fn frames(&self) -> Result<Arc<AudioFrames>, NativeError> {
        Ok(Arc::new(AudioFrames::new(
            self.runtime.clone(),
            self.observer.clone(),
        )?))
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Agent {
    pub fn video(&self, track_id: String) -> Result<Arc<LocalVideoTrack>, NativeError> {
        Ok(Arc::new(LocalVideoTrack {
            runtime: self.runtime.video(track_id)?,
        }))
    }
    pub fn audio(&self, track_id: String) -> Result<Arc<LocalAudioTrack>, NativeError> {
        Ok(Arc::new(LocalAudioTrack {
            runtime: self.runtime.audio(track_id)?,
        }))
    }
    pub async fn create_video_input(
        &self,
        configuration: crate::VideoSourceConfig,
    ) -> Result<Arc<VideoSource>, NativeError> {
        Ok(Arc::new(VideoSource {
            runtime: self.runtime.create_video_input(configuration).await?,
        }))
    }
    pub async fn create_audio_input(&self) -> Result<Arc<AudioSource>, NativeError> {
        Ok(Arc::new(AudioSource {
            runtime: self.runtime.create_audio_input().await?,
        }))
    }
}

#[uniffi::export]
impl Participant {
    pub fn video(&self, track_id: String) -> Result<Arc<RemoteVideoTrack>, NativeError> {
        Ok(Arc::new(RemoteVideoTrack {
            runtime: self.runtime.video(track_id)?,
        }))
    }
    pub fn audio(&self, track_id: String) -> Result<Arc<RemoteAudioTrack>, NativeError> {
        Ok(Arc::new(RemoteAudioTrack {
            runtime: self.runtime.audio(track_id)?,
        }))
    }
}
