//! Stock OpenH264/FFmpeg native media at 720p.
#[path = "support/managed_video.rs"]
mod managed_video;

#[test]
fn high_resolution_stock_h264_uses_managed_call_defaults_and_replays() {
    managed_video::check_replay(
        "H264",
        &[("packetization-mode", "1"), ("profile-level-id", "42e01f")],
        "PULSEBEAM_H264_HOST_WORKER_PROBE",
        "PULSEBEAM_H264_TRACE_PATH",
    );
}

#[test]
fn same_receiver_observes_ordered_h264_and_native_i420_without_interception() {
    managed_video::check_dual_observation();
}

#[test]
fn decoded_callbacks_precede_polling_loss_and_retire_safely() {
    managed_video::check_decoded_callbacks();
}

#[test]
fn encoded_callbacks_survive_polling_loss_and_retire_safely() {
    managed_video::check_encoded_callbacks();
}
