use super::*;
use agent_core::ffi::{ErrorCode, WatchStatus};
use std::future::Future;

fn control() -> Control {
    Control::new(
        agent_core::Agent::new(agent_core::AgentConfig {
            endpoint: "http://pulsebeam.test".into(),
            token: "credential".into(),
            retry: Default::default(),
            log_level: agent_core::LogLevel::Off,
        })
        .unwrap(),
    )
}

struct Reenter(Control);
impl std::task::Wake for Reenter {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        let _ = self.0.state();
    }
}

#[tokio::test]
async fn watch_handle_is_current_first_and_final_drop_wakes_outside_core_lock() {
    let core = control();
    let watch = Watch::new("pinned".into(), WatchConfig::default(), core.clone()).unwrap();
    let owner = watch.inner.owner;
    let retained = watch.clone();
    let updates = watch.updates().unwrap();
    let initial = updates.next().unwrap().await.unwrap();
    assert_eq!(initial.status, WatchStatus::Loading);
    assert_eq!(initial.room, "pinned");
    assert_eq!(initial.limit, 100);
    assert!(initial.page.is_none());
    let mut pending = Box::pin(updates.next().unwrap());
    let waker = std::task::Waker::from(Arc::new(Reenter(core.clone())));
    assert!(
        pending
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending()
    );
    drop(watch);
    assert_eq!(retained.state(), initial);
    drop(retained);
    let closed = pending.await.unwrap();
    assert_eq!(closed.status, WatchStatus::Closed);
    assert!(closed.page.is_none());
    assert!(updates.next().unwrap().await.is_none());
    assert!(
        core.external(|core| core.catalog_watch_state(owner))
            .is_none()
    );
}

#[test]
fn native_watch_reservation_reuses_core_caps_and_releases_synchronously() {
    let core = control();
    let mut watches = (0..8)
        .map(|_| Watch::new("room".into(), WatchConfig::default(), core.clone()).unwrap())
        .collect::<Vec<_>>();
    let error = Watch::new("room".into(), WatchConfig::default(), core.clone())
        .err()
        .unwrap();
    assert_eq!(error.error_info().code, ErrorCode::CapacityExceeded);
    let watched = watches.pop().unwrap();
    let observers = (0..32)
        .map(|_| watched.updates().unwrap())
        .collect::<Vec<_>>();
    assert!(watched.updates().is_err());
    watched.close();
    let final_state = watched.state();
    watched.close();
    assert_eq!(watched.state(), final_state);
    let replacement = Watch::new("remote".into(), WatchConfig::default(), core.clone()).unwrap();
    assert_eq!(replacement.state().room, "remote");
    assert!(core.state().room.is_none());
    drop(observers);
}

#[tokio::test]
async fn terminal_agent_close_closes_watch_cursors_without_waiting_for_network() {
    let core = control();
    let watch = Watch::new("room".into(), WatchConfig::default(), core.clone()).unwrap();
    let updates = watch.updates().unwrap();
    let _ = updates.next().unwrap().await;
    let pending = updates.next().unwrap();
    core.close_observation();
    assert_eq!(pending.await.unwrap().status, WatchStatus::Closed);
    assert!(updates.next().unwrap().await.is_none());
    watch.close();
}
