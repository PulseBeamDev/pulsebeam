#![cfg(feature = "native")]

use std::time::Duration;

use pulsebeam_webrtc_sys::{
    CameraCaptureStatus, PeerConfiguration, PeerConnectionFactory, PeerErrorKind,
};

#[test]
fn native_audio_opt_in_is_a_device_independent_smoke() {
    // The opt-in is only public for native artifacts. Native can still be
    // unavailable on CI without recording/playout services or hardware.
    match PeerConnectionFactory::builder().native_audio(true).build() {
        Ok(factory) => {
            for recording in [true, false] {
                match factory.audio_devices(recording) {
                    Ok(devices) => {
                        for device in devices {
                            assert!(!device.name.is_empty());
                        }
                    }
                    Err(error) => assert!(!error.message.is_empty()),
                }
            }
            for recording in [true, false] {
                assert!(factory.select_audio_device(recording, u16::MAX).is_err());
            }
            let mut peer = factory
                .create_peer_connection(PeerConfiguration::default())
                .unwrap();
            for recording in [true, false] {
                peer.set_native_audio_enabled(recording, false).unwrap();
                peer.set_native_audio_enabled(recording, true).unwrap();
            }
            peer.close().unwrap();
            assert_eq!(
                peer.set_native_audio_enabled(true, false).unwrap_err().kind,
                PeerErrorKind::Closed
            );
        }
        Err(error) => {
            assert_eq!(error.kind, PeerErrorKind::NativeConstruction);
            assert!(!error.message.is_empty());
        }
    }
}

#[test]
fn camera_enumeration_and_invalid_selection_need_no_hardware() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    match factory.camera_devices() {
        Ok(devices) => {
            for device in devices {
                assert!(!device.id.is_empty());
                assert!(!device.name.is_empty());
                for format in factory.camera_formats(&device.id).unwrap() {
                    assert!(format.width > 0);
                    assert!(format.height > 0);
                    assert!(format.max_fps > 0);
                    // A listed device may be busy or inaccessible in CI.
                    if let Ok(mut capture) =
                        factory.open_camera(&device.id, format.width, format.height, format.max_fps)
                    {
                        assert_ne!(
                            capture.status(Duration::from_secs(3)),
                            CameraCaptureStatus::Closed
                        );
                        capture.stop().unwrap();
                        capture.stop().unwrap();
                        assert_eq!(
                            capture.status(Duration::from_secs(3)),
                            CameraCaptureStatus::Closed
                        );
                        break;
                    }
                }
            }
        }
        Err(error) => assert!(!error.message.is_empty()),
    }
    assert_eq!(
        factory.camera_formats("").unwrap_err().kind,
        PeerErrorKind::InvalidParameter
    );
    assert_eq!(
        factory.open_camera("", 640, 480, 30).err().unwrap().kind,
        PeerErrorKind::InvalidParameter
    );
    assert!(
        factory
            .open_camera("nonexistent-camera", 640, 480, 30)
            .is_err()
    );
    match factory.screen_sources() {
        Ok(screens) => {
            for screen in screens {
                // Some display backends expose unnamed outputs.
                assert_ne!(screen.id, i64::MIN);
            }
        }
        Err(error) => assert!(!error.message.is_empty()),
    }
    assert!(factory.open_screen(i64::MIN).is_err());
    match factory.window_sources() {
        Ok(windows) => {
            for window in windows {
                assert_ne!(window.id, i64::MIN);
            }
        }
        Err(error) => assert!(!error.message.is_empty()),
    }
    assert!(factory.open_window(i64::MIN).is_err());
}

#[test]
fn native_factory_without_device_opt_in_rejects_audio_device_calls() {
    let factory = PeerConnectionFactory::builder().build().unwrap();
    assert_eq!(
        factory.audio_devices(true).unwrap_err().kind,
        PeerErrorKind::UnsupportedOperation
    );
    assert_eq!(
        factory.create_microphone_track("mic").err().unwrap().kind,
        PeerErrorKind::UnsupportedOperation
    );
    let peer = factory
        .create_peer_connection(PeerConfiguration::default())
        .unwrap();
    for recording in [true, false] {
        assert_eq!(
            peer.set_native_audio_enabled(recording, false)
                .unwrap_err()
                .kind,
            PeerErrorKind::UnsupportedOperation
        );
    }
}
