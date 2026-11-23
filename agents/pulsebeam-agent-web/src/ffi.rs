#![allow(
    clippy::disallowed_types,
    reason = "UniFFI exposes custom browser media handles"
)]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebMediaTrack(pub u64);

uniffi::custom_newtype!(WebMediaTrack, u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebMediaStream(pub u64);

uniffi::custom_newtype!(WebMediaStream, u64);

use std::sync::Arc;

use agent_core::ffi::{AgentState, ErrorCode, ErrorInfo};

macro_rules! state_cursor {
    ($name:ident, $state:ident) => {
        state_cursor!($name, $state, $name);
    };
    ($name:ident, $state:ident, $core:ident) => {
        #[derive(uniffi::Object)]
        pub struct $name {
            cursor: agent_core::observer::$core,
        }

        #[uniffi::export]
        impl $name {
            pub async fn next(&self) -> Result<Option<agent_core::ffi::$state>, WebAgentError> {
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
    };
}

mod catalog;
mod data;
mod pipe;
mod publication;
mod video;
use publication::{LocalAudioTrack, LocalVideoTrack};
use video::RoomRef;

#[derive(uniffi::Record)]
pub struct WebAgentConfig {
    pub endpoint: String,
    pub token: String,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum WebAgentError {
    #[error("SDK operation failed: {info:?}")]
    Operation { info: ErrorInfo },
}

impl From<ErrorInfo> for WebAgentError {
    fn from(info: ErrorInfo) -> Self {
        Self::Operation { info }
    }
}

#[derive(uniffi::Object)]
pub struct Agent {
    #[cfg(target_arch = "wasm32")]
    runtime: crate::BrowserRuntime,
}

#[cfg(not(target_arch = "wasm32"))]
fn browser_required() -> WebAgentError {
    ErrorInfo::new(ErrorCode::Internal, "This SDK requires a browser runtime").into()
}

#[uniffi::export]
impl Agent {
    #[uniffi::constructor]
    pub fn new(config: WebAgentConfig) -> Result<Arc<Self>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let runtime = crate::BrowserRuntime::start(
                config.endpoint,
                config.token,
                crate::logger::LogLevel::Warn,
            )?;
            Ok(Arc::new(Self { runtime }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = config;
            Err(browser_required())
        }
    }

    pub fn join(&self, room: String) -> Result<(), WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            self.runtime.join_room(room).map_err(Into::into)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = room;
            Err(browser_required())
        }
    }

    pub fn renew(&self, token: String) -> Result<(), WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            self.runtime.renew_token(token).map_err(Into::into)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = token;
            Err(browser_required())
        }
    }

    pub fn video(self: Arc<Self>, label: String) -> Result<Arc<LocalVideoTrack>, WebAgentError> {
        LocalVideoTrack::new(self, label)
    }

    pub fn audio(self: Arc<Self>, label: String) -> Result<Arc<LocalAudioTrack>, WebAgentError> {
        LocalAudioTrack::new(self, label)
    }

    pub fn room(self: Arc<Self>, name: String) -> Result<Arc<RoomRef>, WebAgentError> {
        RoomRef::new(self, name).map(Arc::new)
    }

    pub async fn close(&self) {
        #[cfg(target_arch = "wasm32")]
        self.runtime.close_locally();
    }

    pub fn state(&self) -> Result<AgentState, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            Ok(self.runtime.current_state())
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(browser_required())
    }

    pub fn updates(&self) -> Result<Arc<AgentUpdates>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let cursor = self.runtime.open_updates().map_err(|_| {
                ErrorInfo::new(ErrorCode::CapacityExceeded, "Observer capacity exceeded")
            })?;
            Ok(Arc::new(AgentUpdates { cursor }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(browser_required())
    }
}

state_cursor!(AgentUpdates, AgentState);
