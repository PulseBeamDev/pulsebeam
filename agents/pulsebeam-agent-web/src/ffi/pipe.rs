use super::{Arc, ErrorCode, ErrorInfo, WebAgentError};
use agent_core::ffi;

#[derive(Clone, Copy)]
pub struct WebInputTrack(pub u64);
uniffi::custom_newtype!(WebInputTrack, u64);

macro_rules! capture_source {
    ($name:ident, $publication:ident, $kind:literal, $hint:literal) => {
        #[derive(uniffi::Object)]
        pub struct $name {
            #[cfg(target_arch = "wasm32")]
            input: platform::Input,
        }

        #[uniffi::export]
        impl $name {
            #[uniffi::constructor]
            pub fn from_track(track: WebInputTrack) -> Result<Arc<Self>, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    let input = platform::Input::new(track, $kind)?;
                    Ok(Arc::new(Self { input }))
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let _ = track;
                    Err(super::browser_required())
                }
            }

            pub fn pipe_to(
                &self,
                publication: Arc<super::publication::$publication>,
            ) -> Result<Arc<Pipe>, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    self.input
                        .attach(&publication.agent.runtime, publication.owner, $hint)
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let _ = publication;
                    Err(super::browser_required())
                }
            }

            pub async fn close(&self) -> Result<(), WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                self.input.close().await?;
                Ok(())
            }
        }
    };
}

capture_source!(VideoSource, VideoPublication, "video", "motion");
capture_source!(AudioSource, AudioPublication, "audio", "speech");

#[derive(uniffi::Object)]
pub struct Pipe {
    #[cfg(target_arch = "wasm32")]
    runtime: crate::browser::publication::BrowserPipe,
}

#[uniffi::export]
impl Pipe {
    pub fn state(&self) -> Result<ffi::PipeState, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            Ok(self.runtime.current_state())
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(super::browser_required())
    }

    pub fn updates(&self) -> Result<Arc<PipeUpdates>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let cursor = self.runtime.open_updates()?;
            Ok(Arc::new(PipeUpdates { cursor }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(super::browser_required())
    }

    pub async fn close(&self) -> Result<(), WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        wasm_bindgen_futures::JsFuture::from(self.runtime.close_pipe()?)
            .await
            .map_err(|_| ErrorInfo::new(ErrorCode::Internal, "Pipe cleanup failed"))?;
        Ok(())
    }
}

state_cursor!(PipeUpdates, PipeState);

#[cfg(target_arch = "wasm32")]
mod platform {
    use super::{Arc, ErrorCode, ErrorInfo, Pipe, WebAgentError, WebInputTrack, ffi};
    use crate::{BrowserRuntime, browser::input_is_live, sender_config::SenderConfig};
    use std::{cell::RefCell, sync::Weak};
    use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
    use web_sys::MediaStreamTrack;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(catch, js_namespace = ["globalThis", "__pulsebeamMediaRegistry"], js_name = takeInput)]
        fn take_input(id: u64) -> Result<JsValue, JsValue>;
    }

    pub(super) struct Input {
        track: MediaStreamTrack,
        pipes: RefCell<Vec<Weak<Pipe>>>,
    }

    impl Input {
        pub(super) fn new(handle: WebInputTrack, kind: &str) -> Result<Self, WebAgentError> {
            let track = take_input(handle.0)
                .ok()
                .and_then(|value| value.dyn_into::<MediaStreamTrack>().ok())
                .filter(|track| track.kind() == kind)
                .ok_or_else(|| {
                    ErrorInfo::new(ErrorCode::InvalidArgument, "Invalid capture track")
                })?;
            Ok(Self {
                track,
                pipes: RefCell::default(),
            })
        }

        pub(super) fn attach(
            &self,
            runtime: &BrowserRuntime,
            owner: u64,
            hint: &str,
        ) -> Result<Arc<Pipe>, WebAgentError> {
            if !input_is_live(&self.track) {
                return Err(ErrorInfo::new(ErrorCode::Closed, "Capture source is closed").into());
            }
            let pipe = Arc::new(Pipe {
                runtime: runtime.attach_publication_pipe(owner)?,
            });
            let mut pipes = self.pipes.borrow_mut();
            pipes.retain(|old| {
                old.upgrade().is_some_and(|old| {
                    old.runtime.current_state().status != ffi::PipeStatus::Closed
                })
            });
            pipes.push(Arc::downgrade(&pipe));
            drop(pipes);
            let track = Clone::clone(&self.track);
            let config = SenderConfig {
                content_hint: hint.into(),
                degradation_preference: None,
                encodings: Vec::new(),
            };
            let installed = pipe.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if installed
                    .runtime
                    .install_input(track, config)
                    .await
                    .is_err()
                {
                    // The Core records sender failures; retiring stale work is already fenced.
                    installed.runtime.current_state();
                }
            });
            Ok(pipe)
        }

        pub(super) async fn close(&self) -> Result<(), WebAgentError> {
            self.track.stop();
            let pipes = std::mem::take(&mut *self.pipes.borrow_mut());
            let mut result = Ok(());
            for pipe in pipes.into_iter().filter_map(|pipe| pipe.upgrade()) {
                if let Err(error) = pipe.close().await {
                    result = Err(error);
                }
            }
            result
        }
    }
}
