//! Stock 8-bit VP9 exercises native row/tile/filter worker paths at 720p.
#[path = "support/managed_video.rs"]
mod managed_video;

#[test]
fn high_resolution_stock_vp9_uses_managed_call_defaults_and_replays() {
    managed_video::check_replay(
        "VP9",
        &[("profile-id", "0")],
        "PULSEBEAM_VP9_HOST_WORKER_PROBE",
        "PULSEBEAM_VP9_TRACE_PATH",
    );
}
