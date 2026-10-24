// Keep this test in its own integration-test process: unrelated test threads
// can still be retiring after a join and invalidate the no-helper-thread check.
use std::time::Duration;

use pulsebeam_webrtc_sys::{
    Environment, FieldTrials, KeyExchangeGroup, KeyExchangeGroups, ManualClock, TaskQueueFactory,
};

#[test]
fn trials_change_native_crypto_selection_under_caller_driven_environment() {
    let before = std::fs::read_dir("/proc/self/task").unwrap().count();
    let clock = ManualClock::new(Duration::ZERO).unwrap();
    let queues = TaskQueueFactory::cooperative(&clock).unwrap();
    let disabled = Environment::builder()
        .task_queue_factory(&queues)
        .field_trials(FieldTrials::parse("WebRTC-EnableDtlsPqc/Disabled/").unwrap())
        .build()
        .unwrap();
    let enabled = Environment::builder()
        .task_queue_factory(&queues)
        .field_trials(FieldTrials::parse("WebRTC-EnableDtlsPqc/Enabled/").unwrap())
        .build()
        .unwrap();
    let mut disabled_groups = KeyExchangeGroups::new(Vec::new());
    let mut enabled_groups = KeyExchangeGroups::new(Vec::new());
    disabled_groups.update(&disabled, &[]);
    enabled_groups.update(&enabled, &[]);
    assert!(
        !disabled_groups
            .enabled()
            .contains(&KeyExchangeGroup::X25519_MLKEM768)
    );
    assert_eq!(
        enabled_groups.enabled().first(),
        Some(&KeyExchangeGroup::X25519_MLKEM768)
    );
    assert!(enabled_groups.enabled().contains(&KeyExchangeGroup::X25519));
    assert_eq!(
        std::fs::read_dir("/proc/self/task").unwrap().count(),
        before
    );
}
