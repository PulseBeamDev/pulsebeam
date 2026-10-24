use pulsebeam_webrtc_sys::*;
use std::{
    cell::{Cell, RefCell},
    net::Ipv4Addr,
    rc::Rc,
};
#[path = "common/panic_payload.rs"]
mod panic_payload;

fn network() -> Network {
    Network::new(b"custom", b"caller-owned", Ipv4Addr::UNSPECIFIED, 0)
}

#[test]
fn subscriptions_observe_native_identity_and_clone_isolation() {
    let mut source = network();
    let events = Rc::new(RefCell::new(Vec::new()));
    let alias = source.share();
    let mut tokens = Vec::new();
    for kind in [
        NetworkChangeKind::AdapterType,
        NetworkChangeKind::Preference,
        NetworkChangeKind::Slice,
    ] {
        let events = events.clone();
        let expected = alias.share();
        tokens.push(
            source
                .subscribe(kind, move |value| {
                    assert!(value.is_same_network(&expected));
                    events.borrow_mut().push(kind);
                })
                .unwrap(),
        );
    }
    let mut deep = source.clone();
    deep.set_adapter_type(NetworkAdapterType::Ethernet).unwrap();
    assert!(events.borrow().is_empty());
    source
        .set_adapter_type(NetworkAdapterType::Ethernet)
        .unwrap();
    source
        .set_network_preference(NetworkPreference::NotPreferred)
        .unwrap();
    source
        .set_network_slice(NetworkSlice::UnifiedCommunications)
        .unwrap();
    assert_eq!(
        *events.borrow(),
        [
            NetworkChangeKind::AdapterType,
            NetworkChangeKind::Preference,
            NetworkChangeKind::Slice
        ]
    );
    for token in &tokens {
        assert!(token.cancel());
        assert!(!token.cancel());
    }
    source.set_adapter_type(NetworkAdapterType::Wifi).unwrap();
    assert_eq!(events.borrow().len(), 3);
    drop(source);
    drop(alias);
    assert!(tokens.iter().all(|token| !token.has_failed()));
}

#[test]
fn self_cancel_and_callback_drop_reentry_wait_for_native_receiver_cleanup() {
    struct ReenterDrop {
        network: Network,
        dropped: Rc<Cell<usize>>,
    }
    impl Drop for ReenterDrop {
        fn drop(&mut self) {
            let token = self
                .network
                .subscribe(NetworkChangeKind::AdapterType, |_| {})
                .unwrap();
            self.network
                .set_adapter_type(NetworkAdapterType::Wifi)
                .unwrap();
            self.dropped.set(self.dropped.get() + 1);
            drop(token);
        }
    }
    let mut source = network();
    let slot = Rc::new(RefCell::new(None::<NetworkChangeSubscription>));
    let dropped = Rc::new(Cell::new(0));
    let called = Rc::new(Cell::new(0));
    let guard = ReenterDrop {
        network: source.share(),
        dropped: dropped.clone(),
    };
    let callback_slot = slot.clone();
    let callback_called = called.clone();
    let token = source
        .subscribe(NetworkChangeKind::AdapterType, move |mut value| {
            let _ = &guard;
            callback_called.set(callback_called.get() + 1);
            assert!(
                value
                    .subscribe(NetworkChangeKind::AdapterType, |_| {})
                    .is_err()
            );
            assert_eq!(
                value.set_adapter_type(NetworkAdapterType::Wifi),
                Err(NetworkChangeInProgress)
            );
            // This is a real native early-return setter, not recursive Send.
            value
                .set_adapter_type(NetworkAdapterType::Ethernet)
                .unwrap();
            drop(callback_slot.borrow_mut().take());
            assert_eq!(guard.dropped.get(), 0);
        })
        .unwrap();
    *slot.borrow_mut() = Some(token);
    source
        .set_adapter_type(NetworkAdapterType::Ethernet)
        .unwrap();
    assert_eq!(called.get(), 1);
    assert_eq!(dropped.get(), 1);
    assert_eq!(source.adapter_type(), NetworkAdapterType::Wifi);
    source
        .set_adapter_type(NetworkAdapterType::Ethernet)
        .unwrap();
    assert_eq!(called.get(), 1);
}

#[test]
fn callback_and_final_capture_destruction_contain_hostile_payloads() {
    struct PanicDrop;
    impl Drop for PanicDrop {
        fn drop(&mut self) {
            panic_payload::raise();
        }
    }
    let mut source = network();
    let guard = PanicDrop;
    let token = source
        .subscribe(NetworkChangeKind::AdapterType, move |_| {
            let _ = &guard;
            panic_payload::raise();
        })
        .unwrap();
    source
        .set_adapter_type(NetworkAdapterType::Ethernet)
        .unwrap();
    assert!(token.has_failed());
    source.set_adapter_type(NetworkAdapterType::Wifi).unwrap();
    drop(token);
    source
        .set_adapter_type(NetworkAdapterType::Ethernet)
        .unwrap();
}
