use pulsebeam_webrtc_sys::{TransformableFrameError, TransformableVideoFrame};

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
