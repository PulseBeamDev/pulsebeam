#[test]
fn precompiled_bridge_matches_rust() {
    assert_eq!(
        pulsebeam_webrtc_sys::bridge_identity(),
        "pulsebeam-webrtc-sys-bridge-v2"
    );
}
