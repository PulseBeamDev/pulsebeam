use super::*;

#[tokio::test]
async fn terminal_transfer_preserves_error_after_core_slot_and_native_registration_release() {
    use agent_core::ffi::{ErrorCode, ErrorInfo, WatchStatus};
    let core = Control::new(
        agent_core::Agent::new(agent_core::AgentConfig {
            endpoint: "http://pulsebeam.test".into(),
            token: "credential".into(),
            retry: Default::default(),
            log_level: agent_core::LogLevel::Off,
        })
        .unwrap(),
    );
    let (owner, observer) = core
        .open_catalog_watch("room".into(), WatchConfig::default())
        .unwrap();
    let updates = observer.updates().unwrap();
    let mut failed = updates.next().unwrap().await.unwrap();
    let pending = updates.next().unwrap();
    core.external(|core| core.close_catalog_watch(owner));
    failed.status = WatchStatus::Failed;
    failed.error = Some(ErrorInfo::new(
        ErrorCode::PermissionDenied,
        "Catalog watch failed",
    ));
    core.watch_terminated(owner.id(), failed);
    let final_state = pending.await.unwrap();
    assert_eq!(final_state.status, WatchStatus::Failed);
    assert_eq!(
        final_state.error.as_ref().unwrap().code,
        ErrorCode::PermissionDenied
    );
    assert!(final_state.page.is_none());
    assert!(core.watch_observers.lock().unwrap().is_empty());
    assert!(updates.next().unwrap().await.is_none());
    core.close_catalog_watch(owner, &observer);
    assert_eq!(observer.state(), final_state);
}
