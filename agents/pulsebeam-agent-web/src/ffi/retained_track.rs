use super::{ErrorCode, ErrorInfo, WebAgentError, WebMediaTrack};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use web_sys::MediaStreamTrack;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = ["globalThis", "__pulsebeamMediaRegistry"], js_name = retainTrackLease)]
    fn retain_track(track: &MediaStreamTrack) -> Result<u64, JsValue>;
    #[wasm_bindgen(catch, js_namespace = ["globalThis", "__pulsebeamMediaRegistry"], js_name = release)]
    fn release(handle: u64, kind: &str) -> Result<bool, JsValue>;
}

pub(super) struct RetainedTrack(u64);

impl RetainedTrack {
    pub(super) fn new(track: &MediaStreamTrack) -> Result<Self, WebAgentError> {
        retain_track(track).map(Self).map_err(|_| {
            ErrorInfo::new(
                ErrorCode::Internal,
                "Could not register browser media track",
            )
            .into()
        })
    }

    pub(super) fn handle(&self) -> WebMediaTrack {
        WebMediaTrack(self.0)
    }
}

impl Drop for RetainedTrack {
    fn drop(&mut self) {
        let _ = release(self.0, "track");
    }
}
