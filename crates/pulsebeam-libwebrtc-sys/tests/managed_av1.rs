//! Stock libaom AV1 and dav1d through native Calls at 720p.
#[path = "support/managed_video.rs"]
mod managed_video;

#[test]
fn high_resolution_stock_av1_uses_managed_call_defaults_and_replays() {
    managed_video::check_replay(
        "AV1",
        &[],
        "PULSEBEAM_AV1_HOST_WORKER_PROBE",
        "PULSEBEAM_AV1_TRACE_PATH",
    );
}
