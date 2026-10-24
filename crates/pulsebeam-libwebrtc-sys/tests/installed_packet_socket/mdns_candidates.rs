//! Real SDK host-candidate obfuscation, not a public responder-method probe.
use super::*;

const NAME: &[u8] = b"caller-controlled-ice.local";
#[derive(Default)]
struct Observed {
    names: Mutex<Vec<(IpAddress, MdnsNameCompletion)>>,
    removed: AtomicUsize,
    dropped: AtomicUsize,
}
struct Responder(Arc<Observed>);
impl MdnsResponderProvider for Responder {
    fn create_name(&self, address: IpAddress, completion: MdnsNameCompletion) {
        // A selected caller implementation supplies the name. No network mDNS
        // protocol is claimed; delivery must still pass through the SDK port's
        // actual asynchronous name callback before a candidate is published.
        self.0.names.lock().unwrap().push((address, completion));
    }
    fn remove_name(&self, _: IpAddress, completion: MdnsRemovalCompletion) {
        self.0.removed.fetch_add(1, Ordering::SeqCst);
        assert!(completion.complete(true));
    }
}
impl Drop for Responder {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct MdnsManagers {
    ip: Ipv4Addr,
    link: Arc<Link>,
    observed: Arc<Observed>,
}
struct MdnsManager {
    inner: Manager,
    responder: MdnsResponder,
}
impl NetworkManagerFactoryProvider for MdnsManagers {
    fn create(&self, context: NetworkManagerContext) -> Option<Box<dyn NetworkManager>> {
        assert_eq!(
            context
                .environment()
                .field_trials()
                .lookup("Provider-Transport"),
            "Selected"
        );
        let responder = context
            .mdns_responder(Responder(self.observed.clone()))
            .unwrap();
        let mut network = Network::with_adapter(
            b"provider-mdns",
            b"caller mDNS ICE interface",
            Ipv4Addr::new(10, 84, 0, 0),
            24,
            NetworkAdapterType::Ethernet,
        );
        network.add_ip(self.ip);
        // The SDK does not attach the manager's provider to returned networks.
        // This is the same actual network identity the SDK allocator consumes.
        network.set_mdns_responder(Some(&responder));
        self.link.manager_created.fetch_add(1, Ordering::SeqCst);
        Some(Box::new(MdnsManager {
            inner: Manager {
                network,
                signals: context.signals(),
                link: self.link.clone(),
            },
            responder,
        }))
    }
    fn is_controlled(&self) -> bool {
        true
    }
}
impl NetworkManager for MdnsManager {
    fn start_updating(&self) {
        self.inner.start_updating();
    }
    fn stop_updating(&self) {
        self.inner.stop_updating();
    }
    fn networks(&self) -> Vec<Network> {
        self.inner.networks()
    }
    fn any_address_networks(&self) -> Vec<Network> {
        self.inner.any_address_networks()
    }
    fn default_local_address(&self, family: AddressFamily) -> Option<IpAddress> {
        self.inner.default_local_address(family)
    }
    fn mdns_responder(&self) -> Option<MdnsResponder> {
        Some(self.responder.clone())
    }
}

fn start_gathering(
    harness: &Harness,
    peer: &PeerConnection,
    events: &mut Vec<PeerConnectionEvent>,
) {
    let offer = harness
        .completed(peer, peer.create_offer().unwrap(), events)
        .unwrap();
    harness.completed(peer, peer.set_local_description(offer).unwrap(), events);
}
fn wait_for_name(
    harness: &Harness,
    observed: &Observed,
    peer: &mut PeerConnection,
    events: &mut Vec<PeerConnectionEvent>,
) {
    for _ in 0..2_000 {
        events.extend(std::iter::from_fn(|| peer.try_next_event()));
        if !observed.names.lock().unwrap().is_empty() {
            return;
        }
        harness.step();
    }
    panic!("SDK port did not request the selected network's mDNS name");
}

#[test]
fn sdk_port_waits_for_selected_mdns_obfuscates_host_candidates_and_cancels_retired_work() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let certificate = Certificate::generate(CertificateKeyParams::default(), None).unwrap();
    let harness = Harness::new();
    let ip = Ipv4Addr::new(10, 84, 0, 1);
    let observed = Arc::new(Observed::default());
    let sockets = PacketSocketFactory::from_provider(Sockets {
        ip,
        link: harness.link.clone(),
    });
    let managers = NetworkManagerFactory::from_provider(MdnsManagers {
        ip,
        link: harness.link.clone(),
        observed: observed.clone(),
    });
    let factory = PeerConnectionFactory::builder()
        .environment(harness.environment.clone())
        .controlled_driver(&harness.driver)
        .options(FactoryOptions {
            disable_network_monitor: true,
            ssl_max_version: SslProtocolVersion::DTLS_12,
            ..Default::default()
        })
        .network_manager_factory(&managers)
        .socket_factory(&sockets)
        .build()
        .unwrap();
    let configuration = || {
        let mut config = RtcConfiguration::default();
        config.options.certificates.push(certificate.clone());
        config.options.tcp_candidate_policy = TcpCandidatePolicy::Disabled;
        config
    };
    let mut peer = factory
        .create_peer_connection_with_configuration(configuration())
        .unwrap();
    let channel = peer
        .create_data_channel("mdns-gather", Default::default())
        .unwrap();
    let mut events = Vec::new();
    start_gathering(&harness, &peer, &mut events);
    wait_for_name(&harness, &observed, &mut peer, &mut events);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, PeerConnectionEvent::IceCandidate(_))),
        "host candidates must wait for actual name completion"
    );
    let pending = std::mem::take(&mut *observed.names.lock().unwrap());
    assert!(!pending.is_empty());
    for (address, completion) in &pending {
        assert_eq!(*address, IpAddress::from(ip));
        completion.complete(*address, NAME).unwrap();
    }
    for _ in 0..2_000 {
        events.extend(std::iter::from_fn(|| peer.try_next_event()));
        if events.iter().any(|event| {
            matches!(
                event,
                PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
            )
        }) {
            break;
        }
        harness.step();
    }
    assert!(events.iter().any(|event| matches!(
        event,
        PeerConnectionEvent::IceGatheringStateChanged(IceGatheringState::Complete)
    )));
    let descriptions = peer.descriptions().unwrap();
    let description = descriptions
        .pending_local
        .or(descriptions.current_local)
        .unwrap();
    let candidates: Vec<_> = description
        .sdp
        .lines()
        .filter(|line| line.starts_with("a=candidate:"))
        .collect();
    assert!(!candidates.is_empty());
    let name = std::str::from_utf8(NAME).unwrap();
    assert!(
        candidates
            .iter()
            .all(|line| line.contains(name) && !line.contains(&ip.to_string())),
        "actual local SDP must contain only obfuscated host candidates: {candidates:?}"
    );
    assert!(events.iter().any(|event| matches!(event, PeerConnectionEvent::IceCandidate(candidate) if candidate.candidate.contains(name) && !candidate.candidate.contains(&ip.to_string()))));
    // A distinct real port is left waiting for its name. Keep its single-use
    // completion alive past peer and factory retirement, not just past a public
    // create_name callback or successful duplicate submission.
    let mut unfinished = factory
        .create_peer_connection_with_configuration(configuration())
        .unwrap();
    let unfinished_channel = unfinished
        .create_data_channel("pending-mdns", Default::default())
        .unwrap();
    let mut unfinished_events = Vec::new();
    start_gathering(&harness, &unfinished, &mut unfinished_events);
    wait_for_name(&harness, &observed, &mut unfinished, &mut unfinished_events);
    let unfinished_names = std::mem::take(&mut *observed.names.lock().unwrap());
    assert!(!unfinished_names.is_empty());
    assert!(
        !unfinished_events
            .iter()
            .any(|event| matches!(event, PeerConnectionEvent::IceCandidate(_)))
    );
    peer.close().unwrap();
    unfinished.close().unwrap();
    drop((channel, unfinished_channel, peer, unfinished, factory));
    assert_eq!(observed.dropped.load(Ordering::SeqCst), 1);
    for (address, completion) in &unfinished_names {
        assert_eq!(
            completion.complete(*address, NAME),
            Err(MdnsCompletionError::Inactive)
        );
        assert!(!completion.cancel());
    }
    assert_eq!(
        harness.link.manager_created.load(Ordering::SeqCst),
        harness.link.manager_dropped.load(Ordering::SeqCst)
    );
    assert_eq!(
        harness.link.created.load(Ordering::SeqCst),
        harness.link.dropped.load(Ordering::SeqCst)
    );
    assert!(!sockets.has_failed() && !managers.has_failed());
}
