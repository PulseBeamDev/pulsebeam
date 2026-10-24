use pulsebeam_webrtc_sys::*;
#[path = "common/panic_payload.rs"]
mod panic_payload;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread::ThreadId,
    time::Duration,
};

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct Observed {
    panic_drop: bool,
    requests: Mutex<
        Vec<(
            CertificateKeyParams,
            Option<u64>,
            ThreadId,
            CertificateCompletion,
        )>,
    >,
    dropped: AtomicUsize,
}
struct Provider(Arc<Observed>);
impl CertificateGeneratorProvider for Provider {
    fn generate_certificate_async(
        &mut self,
        params: CertificateKeyParams,
        expires_ms: Option<u64>,
        completion: CertificateCompletion,
    ) {
        self.0.requests.lock().unwrap().push((
            params,
            expires_ms,
            std::thread::current().id(),
            completion,
        ));
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
        if self.0.panic_drop {
            panic_payload::raise();
        }
    }
}

#[test]
fn completion_outlives_generator_but_not_the_factory_signaling_queue() {
    run_completion_lifecycle(false);
}
#[test]
fn native_generator_destruction_contains_hostile_payloads_with_pending_work() {
    run_completion_lifecycle(true);
}
fn run_completion_lifecycle(panic_drop: bool) {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    fn send_sync<T: Send + Sync>() {}
    send_sync::<CertificateGenerator>();
    send_sync::<CertificateCompletion>();
    let observed = Arc::new(Observed {
        panic_drop,
        ..Observed::default()
    });
    let signaling = SignalingThread::start().unwrap();
    let (tx, rx) = mpsc::channel();
    assert!(signaling.post(move || tx.send(std::thread::current().id()).unwrap()));
    let signaling_id = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let factory = PeerConnectionFactory::builder()
        .signaling_thread(signaling)
        .build()
        .unwrap();
    let certificate = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let generator = CertificateGenerator::from_provider(Provider(observed.clone()));
    let mut peer = factory
        .create_peer_connection_with_dependencies(
            RtcConfiguration::default(),
            PeerConnectionDependencies::default().certificate_generator(&generator),
        )
        .unwrap();
    drop(generator);
    let (params, expires, thread, completion) = observed.requests.lock().unwrap().pop().unwrap();
    assert_eq!(params, CertificateKeyParams::default());
    assert_eq!(expires, None);
    assert_eq!(thread, signaling_id);
    peer.close().unwrap();
    drop(peer);
    assert_eq!(observed.dropped.load(Ordering::SeqCst), 1);
    // Native callbacks are weak-peer guarded. Provider destruction alone does
    // not cancel its accepted work, as with the SDK's default generator.
    std::thread::spawn(move || completion.complete(Some(&certificate)).unwrap())
        .join()
        .unwrap();
    let mut peer = factory
        .create_peer_connection_with_dependencies(
            RtcConfiguration::default(),
            PeerConnectionDependencies::default()
                .certificate_generator_provider(Provider(observed.clone())),
        )
        .unwrap();
    let (_, _, thread, completion) = observed.requests.lock().unwrap().pop().unwrap();
    assert_eq!(thread, signaling_id);
    peer.close().unwrap();
    drop(peer);
    assert_eq!(observed.dropped.load(Ordering::SeqCst), 2);
    drop(factory);
    assert_eq!(
        std::thread::spawn(move || completion.complete(None))
            .join()
            .unwrap(),
        Err(CertificateCompletionError::Retired)
    );
}

#[test]
fn provider_failure_is_contained_and_latches_diagnostics() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    struct Panics(Arc<Mutex<Option<CertificateCompletion>>>);
    impl CertificateGeneratorProvider for Panics {
        fn generate_certificate_async(
            &mut self,
            _: CertificateKeyParams,
            _: Option<u64>,
            completion: CertificateCompletion,
        ) {
            *self.0.lock().unwrap() = Some(completion);
            panic_payload::raise();
        }
    }
    let pending = Arc::new(Mutex::new(None));
    let generator = CertificateGenerator::from_provider(Panics(pending.clone()));
    let factory = PeerConnectionFactory::builder().build().unwrap();
    let mut peer = factory
        .create_peer_connection_with_dependencies(
            RtcConfiguration::default(),
            PeerConnectionDependencies::default().certificate_generator(&generator),
        )
        .unwrap();
    assert!(generator.has_failed());
    assert_eq!(
        pending.lock().unwrap().take().unwrap().complete(None),
        Err(CertificateCompletionError::AlreadyCompleted)
    );
    peer.close().unwrap();
}

#[test]
fn native_curve_parameters_preserve_invalid_signed_values() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for value in [-1, 1, i32::MIN, i32::MAX] {
        let params = CertificateKeyParams::Ecdsa {
            curve: CertificateCurve::Other(value),
        };
        assert!(!params.is_valid());
        assert!(Certificate::generate(params, None).is_err());
    }
    assert!(
        CertificateKeyParams::Ecdsa {
            curve: CertificateCurve::Other(0)
        }
        .is_valid()
    );
}

#[test]
fn controlled_success_and_failure_only_deliver_when_the_requesting_sequence_pumps() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let certificate = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let fingerprint = certificate.fingerprint("sha-256").unwrap().value;
    let world = ControlledWorld::acquire(0xced7, Duration::from_secs(1)).unwrap();
    let network = world.create_network().unwrap();
    let endpoint = network
        .register_endpoint(std::net::Ipv4Addr::new(10, 78, 0, 1).into())
        .unwrap();
    let factory = world
        .peer_factory_builder()
        .unwrap()
        .options(FactoryOptions {
            disable_network_monitor: true,
            ..Default::default()
        })
        .network_manager(endpoint.network_manager().unwrap())
        .packet_socket_factory(endpoint.packet_socket_factory().unwrap())
        .build()
        .unwrap();
    let observed = Arc::new(Observed::default());
    for success in [true, false] {
        let mut peer = factory
            .create_peer_connection_with_dependencies(
                RtcConfiguration::default(),
                PeerConnectionDependencies::default()
                    .certificate_generator_provider(Provider(observed.clone())),
            )
            .unwrap();
        let channel = peer
            .create_data_channel("custom-certificate", DataChannelConfiguration::default())
            .unwrap();
        let operation = peer.create_offer().unwrap();
        let (_, _, thread, completion) = observed.requests.lock().unwrap().pop().unwrap();
        assert_eq!(thread, std::thread::current().id());
        let owned = success.then(|| certificate.clone());
        std::thread::spawn(move || completion.complete(owned.as_ref()).unwrap())
            .join()
            .unwrap();
        assert!(
            !std::iter::from_fn(|| peer.try_next_event()).any(|event| matches!(event,
            PeerConnectionEvent::OperationComplete(done) if done.operation_id == operation)),
            "native callback must not run inline or on Rust's completing thread"
        );
        let mut result = None;
        for _ in 0..8 {
            world.pump(256);
            while let Some(event) = peer.try_next_event() {
                if let PeerConnectionEvent::OperationComplete(done) = event
                    && done.operation_id == operation
                {
                    result = Some(done.result);
                }
            }
            if result.is_some() {
                break;
            }
        }
        let result = result.expect("controlled generation callback did not deliver");
        if success {
            let offer = result.unwrap().unwrap();
            assert!(
                offer
                    .sdp
                    .contains(&format!("a=fingerprint:sha-256 {fingerprint}"))
            );
        } else {
            assert!(
                result.is_err(),
                "null native certificate must fail the pending offer"
            );
        }
        peer.close().unwrap();
        drop(channel);
        drop(peer);
        world.pump(256);
    }
    assert_eq!(observed.dropped.load(Ordering::SeqCst), 2);
}
