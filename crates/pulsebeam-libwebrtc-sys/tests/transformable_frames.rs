use pulsebeam_webrtc_sys::{TransformableFrameError, TransformableVideoFrame};

#[path = "support/managed_video.rs"]
mod managed_video;

#[test]
fn sender_metadata_cannot_change_packetizer_codec() {
    managed_video::check_sender_metadata_packetizer_codec();
}

#[test]
fn stock_video_constructors_report_native_unavailability_without_aborting() {
    // Both pinned factory functions terminate with RTC_CHECK_NOTREACHED. This
    // public boundary must not enter them or manufacture a substitute frame.
    assert!(matches!(
        TransformableVideoFrame::create_sender(),
        Err(TransformableFrameError::UnsupportedVideoConstructor)
    ));
    assert!(matches!(
        TransformableVideoFrame::create_receiver(),
        Err(TransformableFrameError::UnsupportedVideoConstructor)
    ));
}
