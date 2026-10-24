//! High-resolution stock VP8 through native Calls, not prerecorded input.
#[path = "support/managed_video.rs"]
mod managed_video;

#[test]
fn high_resolution_stock_vp8_uses_managed_call_defaults_and_replays() {
    managed_video::check_replay(
        "VP8",
        &[],
        "PULSEBEAM_VP8_HOST_WORKER_PROBE",
        "PULSEBEAM_VP8_TRACE_PATH",
    );
}
