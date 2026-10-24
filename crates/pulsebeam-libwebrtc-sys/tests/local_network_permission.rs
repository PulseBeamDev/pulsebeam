use pulsebeam_webrtc_sys::*;
#[path = "common/panic_payload.rs"]
mod panic_payload;
use std::{
    net::{Ipv4Addr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Pending,
    Deny,
    Skip,
    PanicShould,
    PanicRequest,
    PanicFactory,
    PanicDrop,
}
struct Observed {
    mode: Mode,
    created: AtomicUsize,
    requests: AtomicUsize,
    dropped: AtomicUsize,
    factory_dropped: AtomicUsize,
    gathered: std::sync::atomic::AtomicBool,
    threads: Mutex<Vec<std::thread::ThreadId>>,
    pending: Mutex<Vec<(SocketAddress, LocalNetworkPermissionCompletion)>>,
}
impl Observed {
    fn record(&self) {
        self.threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
    }
}
struct Factory(Arc<Observed>);
struct Permission(Arc<Observed>);
impl LocalNetworkPermissionFactory for Factory {
    fn create(&self) -> Arc<dyn LocalNetworkPermission> {
        self.0.record();
        self.0.created.fetch_add(1, Ordering::SeqCst);
        if self.0.mode == Mode::PanicFactory {
            panic_payload::raise();
        }
        Arc::new(Permission(self.0.clone()))
    }
}
impl Drop for Factory {
    fn drop(&mut self) {
        self.0.factory_dropped.fetch_add(1, Ordering::SeqCst);
        if self.0.mode == Mode::PanicDrop {
            panic_payload::raise();
        }
    }
}
impl Drop for Permission {
    fn drop(&mut self) {
        self.0.record();
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
        if self.0.mode == Mode::PanicDrop {
            panic_payload::raise();
        }
    }
}
impl LocalNetworkPermission for Permission {
    fn should_request_permission(&self, address: &SocketAddress) -> bool {
        self.0.record();
        if self.0.mode == Mode::PanicShould {
            panic_payload::raise();
        }
        self.0.mode != Mode::Skip && (address.is_private_ip() || address.is_loopback_ip())
    }
    fn request_permission(
        &self,
        address: SocketAddress,
        completion: LocalNetworkPermissionCompletion,
    ) {
        self.0.record();
        self.0.requests.fetch_add(1, Ordering::SeqCst);
        self.0
            .pending
            .lock()
            .unwrap()
            .push((address, completion.clone()));
        if self.0.mode == Mode::PanicRequest {
            panic_payload::raise();
        }
        if self.0.mode == Mode::Deny {
            assert!(completion.deny());
            assert!(!completion.grant());
        }
    }
}
fn finish(
    session: &mut ProductionSession,
    peer: SessionPeerId,
    operation: OperationId,
    observed: &Observed,
) -> Option<SessionDescription> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        while let Some(event) = session.try_peer_event(peer).unwrap() {
            match event {
                SessionEvent::OperationComplete(done) if done.operation_id == operation => {
                    return done.result.unwrap();
                }
                SessionEvent::IceGatheringStateChanged(IceGatheringState::Complete) => {
                    observed.gathered.store(true, Ordering::SeqCst)
                }
                _ => (),
            }
        }
        assert!(Instant::now() < deadline, "operation did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn start(mode: Mode, port: u16) -> (ProductionSession, SessionPeerId, Arc<Observed>) {
    let observed = Arc::new(Observed {
        mode,
        created: 0.into(),
        requests: 0.into(),
        dropped: 0.into(),
        factory_dropped: 0.into(),
        gathered: false.into(),
        threads: Mutex::new(Vec::new()),
        pending: Mutex::new(Vec::new()),
    });
    let session = ProductionSession::new(ProductionSessionConfig::default()).unwrap();
    let observed_actor = observed.clone();
    let (session, peer) = std::thread::spawn(move || {
        let mut session = session;
        // The pinned default peer factory installs this provider in the ICE
        // transport, not in its BasicPortAllocator's STUN-server queries.
        let remote = session.create_peer(PeerConfiguration::default()).unwrap();
        session
            .create_channel(remote, "permission", DataChannelConfiguration::default())
            .unwrap();
        let operation = session.create_offer(remote, false).unwrap();
        let offer = finish(&mut session, remote, operation, &observed_actor).unwrap();
        let mid = offer
            .sdp
            .lines()
            .find_map(|line| line.strip_prefix("a=mid:"))
            .unwrap()
            .to_owned();
        let dependencies = PeerConnectionDependencies::default()
            .local_network_permission_factory(Factory(observed_actor.clone()));
        let peer = session
            .create_peer_with_dependencies(RtcConfiguration::default(), dependencies)
            .unwrap();
        let operation = session.set_remote_description(peer, offer).unwrap();
        finish(&mut session, peer, operation, &observed_actor);
        let operation = session.create_answer(peer).unwrap();
        let answer = finish(&mut session, peer, operation, &observed_actor).unwrap();
        let operation = session.set_local_description(peer, answer).unwrap();
        finish(&mut session, peer, operation, &observed_actor);
        assert_eq!(
            session
                .add_ice_candidate_sync(
                    peer,
                    IceCandidate {
                        sdp_mid: mid,
                        sdp_mline_index: 0,
                        candidate: format!(
                            "candidate:1 1 UDP 2122260223 127.0.0.1 {port} typ host"
                        ),
                    }
                )
                .unwrap(),
            IceCandidateDisposition::Accepted
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        while observed_actor.created.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "native did not query remote-candidate permission"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        (session, peer)
    })
    .join()
    .unwrap();
    (session, peer, observed)
}
fn retire(session: ProductionSession, peer: SessionPeerId, observed: &Observed) {
    std::thread::spawn(move || {
        let mut session = session;
        session.close_peer(peer).unwrap();
        session.shutdown().unwrap();
    })
    .join()
    .unwrap();
    assert_eq!(observed.factory_dropped.load(Ordering::SeqCst), 1);
    if observed.mode != Mode::PanicFactory {
        assert_eq!(
            observed.created.load(Ordering::SeqCst),
            observed.dropped.load(Ordering::SeqCst)
        );
    }
    let threads = observed.threads.lock().unwrap();
    assert!(
        threads.iter().all(|thread| *thread == threads[0]),
        "native product callbacks and teardown must share a sequence"
    );
    for (address, token) in observed.pending.lock().unwrap().iter() {
        assert_eq!(address.ip_address(), IpAddress::from(Ipv4Addr::LOCALHOST));
        assert!(
            !token.grant(),
            "retired native products must reject retained completions"
        );
    }
}
#[test]
fn actor_permission_completes_from_another_thread_and_retains_owned_addresses() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<LocalNetworkPermissionCompletion>();
    let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let (session, peer, observed) = start(Mode::Pending, server.local_addr().unwrap().port());
    let deadline = Instant::now() + Duration::from_secs(15);
    while observed.pending.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "permission request did not arrive"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let token = observed.pending.lock().unwrap()[0].1.clone();
    let address = observed.pending.lock().unwrap()[0].0.clone();
    std::thread::spawn(move || {
        assert!(token.grant());
        assert!(!token.deny());
    })
    .join()
    .unwrap();
    let mut bytes = [0; 2048];
    assert!(
        server.recv_from(&mut bytes).unwrap().0 >= 20,
        "grant must release native candidate connectivity checks"
    );
    retire(session, peer, &observed);
    std::thread::spawn(move || {
        assert_eq!(address.ip_address(), IpAddress::from(Ipv4Addr::LOCALHOST))
    })
    .join()
    .unwrap();
}
#[test]
fn actor_permission_retirement_cancels_unanswered_completion() {
    retire_pending(Mode::Pending);
}
#[test]
fn actor_permission_factory_and_product_drops_contain_hostile_payloads() {
    retire_pending(Mode::PanicDrop);
}
fn retire_pending(mode: Mode) {
    let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let (session, peer, observed) = start(mode, server.local_addr().unwrap().port());
    let deadline = Instant::now() + Duration::from_secs(15);
    while observed.pending.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let token = observed.pending.lock().unwrap()[0].1.clone();
    retire(session, peer, &observed);
    std::thread::spawn(move || assert!(!token.deny()))
        .join()
        .unwrap();
}
#[test]
fn denial_and_provider_panics_reject_native_candidate_checks_while_skip_bypasses_request() {
    for mode in [
        Mode::Deny,
        Mode::PanicShould,
        Mode::PanicRequest,
        Mode::PanicFactory,
        Mode::Skip,
    ] {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let (session, peer, observed) = start(mode, server.local_addr().unwrap().port());
        if mode == Mode::Skip {
            server
                .set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            let mut bytes = [0; 2048];
            assert!(server.recv_from(&mut bytes).unwrap().0 >= 20);
            assert_eq!(observed.requests.load(Ordering::SeqCst), 0);
        } else {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut gathered = observed.gathered.load(Ordering::SeqCst);
            let mut session = session;
            while !gathered {
                while let Some(event) = session.try_peer_event(peer).unwrap() {
                    gathered |= matches!(
                        event,
                        SessionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
                    );
                }
                assert!(
                    Instant::now() < deadline,
                    "denied native gathering did not finish"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let mut bytes = [0; 2048];
            assert!(
                server.recv_from(&mut bytes).is_err(),
                "denial must not send connectivity checks to the candidate"
            );
            if matches!(mode, Mode::PanicShould | Mode::PanicFactory) {
                assert_eq!(observed.requests.load(Ordering::SeqCst), 0);
            }
            retire(session, peer, &observed);
            continue;
        }
        retire(session, peer, &observed);
    }
}
