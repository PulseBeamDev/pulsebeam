#![cfg(all(feature = "native", target_os = "linux"))]

use pulsebeam_webrtc_sys::*;

#[test]
fn declaration_only_static_create_is_not_a_null_or_helper_substitution() {
    let environment = Environment::builder().build().unwrap();
    let worker = WorkerThread::start().unwrap();
    assert!(matches!(
        AudioDeviceModule::create(&environment, worker, AudioDeviceLayer::Dummy, false),
        Err(AudioDeviceError::UnlinkedStaticCreate)
    ));
}

#[test]
#[ignore = "requires live PulseAudio/PipeWire service; metadata only, no recording or playout"]
fn live_pulse_metadata_uses_the_selected_backend_without_starting_audio() {
    let environment = Environment::builder().build().unwrap();
    let worker = WorkerThread::start().unwrap();
    let mut module =
        AudioDeviceModule::create_with_helper(&environment, worker, AudioDeviceLayer::LinuxPulse)
            .unwrap()
            .expect("explicit PulseAudio module");
    module.init().unwrap();
    assert!(module.initialized());
    assert_eq!(
        module.active_audio_layer().unwrap(),
        AudioDeviceLayer::LinuxPulse
    );
    let playout = module.playout_device_count().unwrap();
    let recording = module.recording_device_count().unwrap();
    println!("native Pulse metadata: {playout} playout devices, {recording} recording devices");
    module.terminate().unwrap();
    assert!(!module.initialized());
}

// Explicit SDK dummy selection, not a substitute for physical backend evidence.
#[test]
fn native_free_factory_retains_worker_environment_and_lifecycle() {
    for bypass in [false, true] {
        let environment = Environment::builder().build().unwrap();
        let worker = WorkerThread::start().unwrap();
        let mut module = AudioDeviceModule::create_with_helper_options(
            &environment,
            worker,
            AudioDeviceLayer::Dummy,
            bypass,
        )
        .unwrap()
        .expect("explicit SDK dummy audio module");
        drop(environment);
        assert!(!module.initialized());
        module.init().unwrap();
        assert!(module.initialized());
        module.terminate().unwrap();
        assert!(!module.initialized());
        drop(module);
    }
}
