use pulsebeam_webrtc_sys::*;
#[path = "common/panic_payload.rs"]
mod panic_payload;
use std::{
    net::Ipv4Addr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, AtomicUsize, Ordering},
    },
    time::Duration,
};

static TEST_LOCK: Mutex<()> = Mutex::new(());
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum DropPoint {
    #[default]
    None,
    Factory,
    Resolver,
    Result,
}
#[derive(Default)]
struct Observed {
    drop_point: DropPoint,
    factory_dropped: AtomicUsize,
    result_dropped: AtomicUsize,
    address: Mutex<Option<SocketAddress>>,
    pending: Mutex<Vec<(AddressFamily, TaskQueueContext, DnsResolutionCompletion)>>,
    calls: Mutex<Vec<(AddressFamily, SocketAddress)>>,
    families: Mutex<Vec<AddressFamily>>,
    query_contexts: Mutex<Vec<TaskQueueContext>>,
    error: AtomicI32,
    errors_queried: AtomicUsize,
    created: AtomicUsize,
    dropped: AtomicUsize,
}
struct ResultProvider(Arc<Observed>);
impl Drop for ResultProvider {
    fn drop(&mut self) {
        self.0.result_dropped.fetch_add(1, Ordering::SeqCst);
        if self.0.drop_point == DropPoint::Result {
            panic_payload::raise();
        }
    }
}
impl AsyncDnsResolverResult for ResultProvider {
    fn resolved_address(&self, family: AddressFamily) -> Option<SocketAddress> {
        self.0.families.lock().unwrap().push(family);
        let ip = IpAddress::from(Ipv4Addr::new(10, 79, 0, 2));
        if ip.family() != family {
            return None;
        }
        let mut address = self.0.address.lock().unwrap().clone()?;
        address.set_resolved_ip(ip);
        self.0.calls.lock().unwrap().push((family, address.clone()));
        Some(address)
    }
    fn error(&self) -> i32 {
        if let Some(context) = TaskQueueContext::current() {
            self.0.query_contexts.lock().unwrap().push(context);
        }
        self.0.errors_queried.fetch_add(1, Ordering::SeqCst);
        self.0.error.load(Ordering::SeqCst)
    }
}
struct Resolver {
    observed: Arc<Observed>,
    result: DnsResolverResult,
}
impl AsyncDnsResolver for Resolver {
    fn start_with_family(
        &mut self,
        address: SocketAddress,
        family: AddressFamily,
        completion: DnsResolutionCompletion,
    ) {
        *self.observed.address.lock().unwrap() = Some(address);
        self.observed.pending.lock().unwrap().push((
            family,
            TaskQueueContext::current().expect("request must have a native queue"),
            completion,
        ));
    }
    fn result(&self) -> DnsResolverResult {
        self.result.clone()
    }
}
impl Drop for Resolver {
    fn drop(&mut self) {
        self.observed.dropped.fetch_add(1, Ordering::SeqCst);
        if self.observed.drop_point == DropPoint::Resolver {
            panic_payload::raise();
        }
    }
}
struct Factory(Arc<Observed>);
impl Drop for Factory {
    fn drop(&mut self) {
        self.0.factory_dropped.fetch_add(1, Ordering::SeqCst);
        if self.0.drop_point == DropPoint::Factory {
            panic_payload::raise();
        }
    }
}
impl AsyncDnsResolverFactoryProvider for Factory {
    fn create(&self) -> Box<dyn AsyncDnsResolver> {
        self.0.created.fetch_add(1, Ordering::SeqCst);
        Box::new(Resolver {
            observed: self.0.clone(),
            result: DnsResolverResult::from_provider(ResultProvider(self.0.clone())),
        })
    }
}

#[test]
fn callable_result_preserves_family_hostname_port_and_signed_errors() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<DnsResolverResult>();
    send_sync::<DnsResolutionCompletion>();
    send_sync::<AsyncDnsResolverFactory>();
    let observed = Arc::new(Observed::default());
    *observed.address.lock().unwrap() =
        Some(SocketAddress::from_host(b"dns.example.invalid", 3478));
    let product = Factory(observed.clone()).create();
    let result = product.result();
    let retained = result.clone();
    drop(product);
    assert_eq!(retained.error(), 0);
    for code in [-1, -31, i32::MIN, i32::MAX] {
        observed.error.store(code, Ordering::SeqCst);
        assert_eq!(
            retained.error(),
            code,
            "result handles must not cache error snapshots"
        );
    }
    assert!(result.resolved_address(AddressFamily::Ipv6).is_none());
    assert!(result.resolved_address(AddressFamily::Other(-97)).is_none());
    let address = retained.resolved_address(AddressFamily::Ipv4).unwrap();
    assert_eq!(address.hostname(), b"dns.example.invalid");
    assert_eq!(address.port(), 3478);
    assert_eq!(
        address.ip_address(),
        IpAddress::from(Ipv4Addr::new(10, 79, 0, 2))
    );
    assert_eq!(
        *observed.families.lock().unwrap(),
        [
            AddressFamily::Ipv6,
            AddressFamily::Other(-97),
            AddressFamily::Ipv4
        ]
    );
    for family in [
        AddressFamily::Unspecified,
        AddressFamily::Ipv4,
        AddressFamily::Ipv6,
        AddressFamily::Other(-97),
    ] {
        assert_eq!(
            AddressFamily::from_native_value(family.native_value()),
            family
        );
    }
}

#[test]
fn result_panics_latch_and_non_sync_owned_providers_are_supported() {
    struct Panics(std::cell::Cell<usize>);
    impl AsyncDnsResolverResult for Panics {
        fn resolved_address(&self, _: AddressFamily) -> Option<SocketAddress> {
            panic_payload::raise()
        }
        fn error(&self) -> i32 {
            self.0.set(self.0.get() + 1);
            panic_payload::raise()
        }
    }
    for address_first in [true, false] {
        let result = DnsResolverResult::from_provider(Panics(std::cell::Cell::new(0)));
        if address_first {
            assert!(result.resolved_address(AddressFamily::Ipv4).is_none());
        } else {
            assert_eq!(result.error(), -1);
        }
        assert!(result.has_failed());
        assert_eq!(result.error(), -1);
        assert!(result.resolved_address(AddressFamily::Ipv6).is_none());
    }
}

#[test]
fn native_hostname_candidates_query_live_results_only_after_controlled_origin_delivery() {
    run_native_dns(DropPoint::None);
}
#[test]
fn native_dns_factory_resolver_and_result_drops_contain_hostile_payloads() {
    for drop_point in [DropPoint::Factory, DropPoint::Resolver, DropPoint::Result] {
        run_native_dns(drop_point);
    }
}
fn run_native_dns(drop_point: DropPoint) {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let certificate = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let world = ControlledWorld::acquire(0xd05, Duration::from_secs(1)).unwrap();
    let network = world.create_network().unwrap();
    let endpoint = network
        .register_endpoint(Ipv4Addr::new(10, 79, 0, 1).into())
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
    fn completed(
        world: &ControlledWorld,
        peer: &PeerConnection,
        id: OperationId,
    ) -> Option<SessionDescription> {
        for _ in 0..16 {
            world.pump(256);
            while let Some(event) = peer.try_next_event() {
                if let PeerConnectionEvent::OperationComplete(done) = event
                    && done.operation_id == id
                {
                    return done.result.unwrap();
                }
            }
        }
        panic!("controlled operation did not complete");
    }
    let mut configuration = RtcConfiguration::default();
    configuration.options.certificates.push(certificate.clone());
    let mut remote = factory
        .create_peer_connection_with_configuration(configuration.clone())
        .unwrap();
    let remote_channel = remote
        .create_data_channel("async-dns", DataChannelConfiguration::default())
        .unwrap();
    let remote_offer = completed(&world, &remote, remote.create_offer().unwrap()).unwrap();
    let mid = remote_offer
        .sdp
        .lines()
        .find_map(|line| line.strip_prefix("a=mid:"))
        .unwrap()
        .to_owned();
    for retire_before_delivery in [false, true] {
        let observed = Arc::new(Observed {
            drop_point,
            ..Observed::default()
        });
        let dns = AsyncDnsResolverFactory::from_provider(Factory(observed.clone()));
        let mut peer = factory
            .create_peer_connection_with_dependencies(
                configuration.clone(),
                PeerConnectionDependencies::default().async_dns_resolver_factory(&dns),
            )
            .unwrap();
        completed(
            &world,
            &peer,
            peer.set_remote_description(remote_offer.clone()).unwrap(),
        );
        assert_eq!(
            peer.add_ice_candidate_sync(IceCandidate {
                sdp_mid: mid.clone(),
                sdp_mline_index: 0,
                candidate: "candidate:1 1 UDP 2122260223 async-dns.example.invalid 3478 typ host"
                    .into(),
            })
            .unwrap(),
            IceCandidateDisposition::Accepted
        );
        for _ in 0..16 {
            world.pump(256);
            if !observed.pending.lock().unwrap().is_empty() {
                break;
            }
        }
        let pending = std::mem::take(&mut *observed.pending.lock().unwrap());
        assert!(
            !pending.is_empty(),
            "native hostname candidate resolution did not use the injected DNS factory"
        );
        for (family, _, _) in &pending {
            assert_eq!(
                *family,
                AddressFamily::Unspecified,
                "no-family Start must use the unresolved input's native family"
            );
        }
        let origins: Vec<_> = pending
            .iter()
            .map(|(_, context, _)| context.clone())
            .collect();
        let original = observed.address.lock().unwrap().clone().unwrap();
        assert_eq!(original.hostname(), b"async-dns.example.invalid");
        assert_eq!(original.port(), 3478);
        assert_eq!(observed.errors_queried.load(Ordering::SeqCst), 0);
        if retire_before_delivery {
            drop(dns);
            peer.close().unwrap();
            drop(peer);
            for (_, _, completion) in pending {
                assert_eq!(
                    std::thread::spawn(move || completion.complete())
                        .join()
                        .unwrap(),
                    Err(DnsResolutionCompletionError::Retired)
                );
            }
            assert_eq!(
                observed.created.load(Ordering::SeqCst),
                observed.dropped.load(Ordering::SeqCst)
            );
        } else {
            for (_, origin, completion) in pending {
                assert!(!origin.name_bytes().is_empty());
                std::thread::spawn(move || completion.complete().unwrap())
                    .join()
                    .unwrap();
            }
            assert_eq!(
                observed.errors_queried.load(Ordering::SeqCst),
                0,
                "completion must neither query nor notify inline"
            );
            for _ in 0..16 {
                world.pump(256);
                if observed.errors_queried.load(Ordering::SeqCst) != 0 {
                    break;
                }
            }
            assert!(observed.errors_queried.load(Ordering::SeqCst) != 0);
            assert_eq!(
                *observed.families.lock().unwrap(),
                [AddressFamily::Ipv6, AddressFamily::Ipv4].repeat(origins.len()),
                "native candidate consumers prefer IPv6 and fall back to IPv4"
            );
            assert!(
                observed
                    .query_contexts
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|query| origins
                        .iter()
                        .any(|origin| origin.identity() == query.identity())),
                "notification and result queries must run on the requesting native queue"
            );
            let calls = observed.calls.lock().unwrap();
            assert!(
                !calls.is_empty(),
                "native resolver result must be queried, not converted to an unrelated snapshot"
            );
            assert!(
                calls
                    .iter()
                    .all(|(family, address)| *family == address.family()
                        && address.hostname() == original.hostname()
                        && address.port() == original.port())
            );
            drop(calls);
            let dropped_with_panic = (drop_point == DropPoint::Resolver
                && observed.dropped.load(Ordering::SeqCst) != 0)
                || (drop_point == DropPoint::Result
                    && observed.result_dropped.load(Ordering::SeqCst) != 0);
            assert_eq!(
                dns.has_failed(),
                dropped_with_panic,
                "destructor failure must latch as soon as native resolution retires its owner"
            );
            drop(dns);
            peer.close().unwrap();
            drop(peer);
            assert_eq!(
                observed.created.load(Ordering::SeqCst),
                observed.dropped.load(Ordering::SeqCst)
            );
        }
        world.pump(256);
        assert_eq!(observed.factory_dropped.load(Ordering::SeqCst), 1);
        assert_eq!(
            observed.result_dropped.load(Ordering::SeqCst),
            observed.created.load(Ordering::SeqCst)
        );
    }
    remote.close().unwrap();
    drop(remote_channel);
    drop(remote);
}
