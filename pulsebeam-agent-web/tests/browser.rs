#![cfg(target_arch = "wasm32")]

use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

use pulsebeam_agent_web::interop::{PeerConfig, SIGNALING_LABEL};
use pulsebeam_agent_web::{TransportGeneration, WebTransport};

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn browser_fixture_has_window_and_core_peer_defaults() {
    assert!(web_sys::window().is_some());
    assert_eq!(PeerConfig::default().bundle_policy, "max-bundle");
    assert_eq!(SIGNALING_LABEL, "v1/sys/signaling");
}

#[wasm_bindgen_test]
fn browser_transport_keeps_generation_value_owned() {
    let mut transport = WebTransport::new(PeerConfig::default()).expect("peer config is valid");
    transport
        .connect(TransportGeneration::new(1))
        .expect("first browser generation connects");
    assert_eq!(transport.generation(), Some(TransportGeneration::new(1)));
}
