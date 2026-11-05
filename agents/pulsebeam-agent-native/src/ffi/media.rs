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
            pub async fn ready(&self) -> Result<(), NativeError> {
                self.runtime.ready().await.map_err(Into::into)
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
    ($name:ident, $source:ident) => {
        #[derive(uniffi::Object)]
        pub struct $name {
            runtime: RuntimePublication,
        }

        #[uniffi::export(async_runtime = "tokio")]
        impl $name {
            pub fn is_closed(&self) -> bool {
                self.runtime.is_closed()
            }
            pub async fn ready(&self) -> Result<(), NativeError> {
                self.runtime.ready().await.map_err(Into::into)
            }
            pub async fn set_source(
                &self,
                source: Option<Arc<$source>>,
            ) -> Result<(), NativeError> {
                self.runtime
                    .set_source(source.as_ref().map(|source| &source.runtime))
                    .await
                    .map_err(Into::into)
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
    };
}

publication!(VideoPublication, VideoSource);
publication!(AudioPublication, AudioSource);

macro_rules! local_track {
    ($name:ident, $source:ident, $publication:ident) => {
        #[derive(uniffi::Object)]
        pub struct $name {
            runtime: crate::$name,
        }

        #[uniffi::export(async_runtime = "tokio")]
        impl $name {
            pub async fn publish(
                &self,
                source: Arc<$source>,
                configuration: core_ffi::PublicationConfig,
            ) -> Result<Arc<$publication>, NativeError> {
                Ok(Arc::new($publication {
                    runtime: self.runtime.publish(&source.runtime, configuration).await?,
                }))
            }
        }
    };
}

local_track!(LocalVideoTrack, VideoSource, VideoPublication);
local_track!(LocalAudioTrack, AudioSource, AudioPublication);

#[derive(uniffi::Object)]
pub struct RemoteVideoTrack {
    runtime: crate::RemoteVideoTrack,
}

#[uniffi::export(async_runtime = "tokio")]
impl RemoteVideoTrack {
    pub async fn subscribe(
        &self,
        configuration: core_ffi::VideoObservation,
    ) -> Result<Arc<VideoSubscription>, NativeError> {
        Ok(Arc::new(VideoSubscription {
            runtime: self.runtime.subscribe(configuration.into()).await?,
        }))
    }
}

#[derive(uniffi::Object)]
pub struct RemoteAudioTrack {
    runtime: crate::RemoteAudioTrack,
}

#[uniffi::export(async_runtime = "tokio")]
impl RemoteAudioTrack {
    pub async fn subscribe(
        &self,
        configuration: core_ffi::AudioSubscriptionConfig,
    ) -> Result<Arc<AudioSubscription>, NativeError> {
        Ok(Arc::new(AudioSubscription {
            runtime: self.runtime.subscribe(configuration).await?,
        }))
    }
}

macro_rules! subscription {
    ($name:ident) => {
        #[derive(uniffi::Object)]
        pub struct $name {
            runtime: RuntimeSubscription,
        }

        #[uniffi::export(async_runtime = "tokio")]
        impl $name {
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

subscription!(VideoSubscription);
subscription!(AudioSubscription);

#[uniffi::export(async_runtime = "tokio")]
impl VideoSubscription {
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

#[uniffi::export(async_runtime = "tokio")]
impl AudioSubscription {
    pub async fn configure(
        &self,
        configuration: core_ffi::AudioSubscriptionConfig,
    ) -> Result<(), NativeError> {
        self.runtime
            .configure_audio(configuration.playout_delay.into())
            .await
            .map_err(Into::into)
    }
}

#[derive(uniffi::Object)]
pub struct AudioSelection {
    runtime: RuntimeSubscription,
}

#[uniffi::export(async_runtime = "tokio")]
impl AudioSelection {
    pub fn is_closed(&self) -> bool {
        self.runtime.is_closed()
    }
    pub async fn configure(&self, policy: core_ffi::AudioDemand) -> Result<(), NativeError> {
        self.runtime
            .configure_audio_policy(policy.into())
            .await
            .map_err(Into::into)
    }
    pub async fn observe_source(
        &self,
        source: String,
        mode: ReceiveMode,
    ) -> Result<Arc<RemoteMediaReceiver>, NativeError> {
        Ok(Arc::new(RemoteMediaReceiver {
            runtime: self.runtime.observe_source(source, mode).await?,
        }))
    }
    pub async fn close(&self) -> Result<(), NativeError> {
        self.runtime.close().await.map_err(Into::into)
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Agent {
    pub fn video(&self, label: String) -> Result<Arc<LocalVideoTrack>, NativeError> {
        Ok(Arc::new(LocalVideoTrack {
            runtime: self.runtime.video(label)?,
        }))
    }
    pub fn audio(&self, label: String) -> Result<Arc<LocalAudioTrack>, NativeError> {
        Ok(Arc::new(LocalAudioTrack {
            runtime: self.runtime.audio(label)?,
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
    pub async fn select_audio(
        &self,
        policy: core_ffi::AudioDemand,
    ) -> Result<Arc<AudioSelection>, NativeError> {
        Ok(Arc::new(AudioSelection {
            runtime: self.runtime.select_audio(policy.into()).await?,
        }))
    }
}

#[uniffi::export]
impl Participant {
    pub fn video(&self, label: String) -> Result<Arc<RemoteVideoTrack>, NativeError> {
        Ok(Arc::new(RemoteVideoTrack {
            runtime: self.runtime.video(label)?,
        }))
    }
    pub fn audio(&self, label: String) -> Result<Arc<RemoteAudioTrack>, NativeError> {
        Ok(Arc::new(RemoteAudioTrack {
            runtime: self.runtime.audio(label)?,
        }))
    }
}
