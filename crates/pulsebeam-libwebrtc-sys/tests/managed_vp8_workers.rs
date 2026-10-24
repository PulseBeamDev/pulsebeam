//! Actual SDK builtin VP8 at explicit eight-core resource input.
#[path = "support/managed_video.rs"]
mod managed_video;

#[test]
fn stock_vp8_multiworker_uses_shared_kernel_and_replays() {
    managed_video::check_multicore_replay("PULSEBEAM_VP8_WORKERS_TRACE_PATH");
}
