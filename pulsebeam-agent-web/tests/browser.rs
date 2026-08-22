#![cfg(target_arch = "wasm32")]

use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

use pulsebeam_agent_web::interop::{PeerConfig, SIGNALING_LABEL};

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn browser_fixture_has_window_and_core_peer_defaults() {
    assert!(web_sys::window().is_some());
    assert_eq!(PeerConfig::default().bundle_policy, "max-bundle");
    assert_eq!(SIGNALING_LABEL, "v1/sys/signaling");
}
