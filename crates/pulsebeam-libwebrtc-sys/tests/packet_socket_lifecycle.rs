use pulsebeam_webrtc_sys::*;
use std::{
    cell::{Cell, RefCell},
    net::Ipv4Addr,
    num::NonZeroI32,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Observed {
    drops: AtomicUsize,
    reject: AtomicBool,
    events: Mutex<Option<PacketSocketEvents>>,
    sends: Mutex<Vec<(Vec<u8>, AsyncSocketPacketOptions)>>,
}
struct Socket {
    observed: Arc<Observed>,
    error: Cell<i32>,
    on_drop: Option<Box<dyn FnOnce()>>,
}
impl Socket {
    fn new(observed: Arc<Observed>) -> Self {
        Self {
            observed,
            error: Cell::new(-11),
            on_drop: None,
        }
    }
}
impl Drop for Socket {
    fn drop(&mut self) {
        self.observed.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(callback) = self.on_drop.take() {
            callback();
        }
    }
}
fn address(port: u16) -> SocketAddress {
    SocketAddress::new(Ipv4Addr::LOCALHOST, port)
}
impl PacketSocket for Socket {
    fn local_address(&self) -> SocketAddress {
        address(31337)
    }
    fn remote_address(&self) -> SocketAddress {
        address(31338)
    }
    fn send(&self, bytes: &[u8], options: &AsyncSocketPacketOptions) -> i32 {
        self.observed
            .sends
            .lock()
            .unwrap()
            .push((bytes.to_vec(), options.clone()));
        -17
    }
    fn send_to(
        &self,
        bytes: &[u8],
        remote: SocketAddress,
        options: &AsyncSocketPacketOptions,
    ) -> i32 {
        assert_eq!(remote, address(31338));
        self.send(bytes, options)
    }
    fn close(&self) -> i32 {
        -19
    }
    fn state(&self) -> PacketSocketState {
        PacketSocketState::BOUND
    }
    fn get_option(&self, option: PacketSocketOption) -> Result<i32, PacketSocketOptionError> {
        if option == PacketSocketOption::RECEIVE_BUFFER {
            Err(PacketSocketOptionError(NonZeroI32::new(-7).unwrap()))
        } else {
            Ok(-123)
        }
    }
    fn set_option(&self, _: PacketSocketOption, value: i32) -> i32 {
        value
    }
    fn error(&self) -> i32 {
        self.error.get()
    }
    fn set_error(&self, value: i32) {
        self.error.set(value);
    }
}
struct Listener;
impl ListenSocket for Listener {
    fn state(&self) -> ListenSocketState {
        ListenSocketState::BOUND
    }
    fn local_address(&self) -> SocketAddress {
        address(31337)
    }
}
struct Factory(Arc<Observed>);
impl PacketSocketFactoryImplementation for Factory {
    fn create_udp_socket(
        &self,
        context: PacketSocketContext,
        _: SocketAddress,
        _: u16,
        _: u16,
    ) -> Option<Box<dyn PacketSocket>> {
        assert_eq!(
            context.environment().field_trials().lookup("Socket-Test"),
            "EnabledOwned"
        );
        assert_eq!(context.state(), Err(PacketSocketEventError::NotReady));
        *self.0.events.lock().unwrap() = Some(context.events());
        (!self.0.reject.load(Ordering::SeqCst))
            .then(|| Box::new(Socket::new(self.0.clone())) as Box<dyn PacketSocket>)
    }
    fn create_server_tcp_socket(
        &self,
        context: ListenSocketContext,
        _: SocketAddress,
        _: u16,
        _: u16,
        _: PacketSocketFlags,
    ) -> Option<Box<dyn ListenSocket>> {
        assert_eq!(
            context.environment().field_trials().lookup("Socket-Test"),
            "EnabledOwned"
        );
        Some(Box::new(Listener))
    }
    fn create_client_tcp_socket(
        &self,
        context: PacketSocketContext,
        local: SocketAddress,
        _: SocketAddress,
        _: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>> {
        self.create_udp_socket(context, local, 0, 0)
    }
    fn create_client_udp_socket(
        &self,
        context: PacketSocketContext,
        local: SocketAddress,
        _: SocketAddress,
        min: u16,
        max: u16,
        _: PacketSocketTcpOptions,
    ) -> Option<Box<dyn PacketSocket>> {
        self.create_udp_socket(context, local, min, max)
    }
    fn create_async_dns_resolver(&self, _: Environment) -> Box<dyn AsyncDnsResolver> {
        unreachable!("standalone socket fixture does not request DNS")
    }
    fn is_controlled(&self) -> bool {
        true
    }
}
struct Fixture {
    driver: ControlledPeerDriver,
    environment: Environment,
    factory: PacketSocketFactory,
    observed: Arc<Observed>,
}
impl Fixture {
    fn new() -> Self {
        let clock = ManualClock::new(Duration::from_secs(10)).unwrap();
        let driver = ControlledPeerDriver::new(&clock).unwrap();
        let queues = TaskQueueFactory::cooperative(&clock).unwrap();
        let environment = Environment::builder()
            .task_queue_factory(&queues)
            .field_trials(FieldTrials::parse("Socket-Test/EnabledOwned/").unwrap())
            .build()
            .unwrap();
        let observed = Arc::new(Observed::default());
        let factory = PacketSocketFactory::from_provider(Factory(observed.clone()));
        Self {
            driver,
            environment,
            factory,
            observed,
        }
    }
    fn socket(&self) -> OwnedPacketSocket {
        self.factory
            .create_udp_socket_controlled(&self.driver, &self.environment, address(0), 0, 0)
            .unwrap()
            .unwrap()
    }
    fn listener(&self) -> OwnedListenSocket {
        self.factory
            .create_server_tcp_socket_controlled(
                &self.driver,
                &self.environment,
                address(0),
                0,
                0,
                PacketSocketFlags::default(),
            )
            .unwrap()
            .unwrap()
    }
}

#[test]
fn actual_native_members_preserve_results_and_retained_context_retires() {
    let fixture = Fixture::new();
    let socket = fixture.socket();
    let context = socket.context().clone();
    assert_eq!(socket.local_address().unwrap(), address(31337));
    assert_eq!(socket.remote_address().unwrap(), address(31338));
    assert_eq!(socket.state().unwrap(), PacketSocketState::BOUND);
    assert_eq!(socket.error().unwrap(), -11);
    socket.set_error(i32::MIN).unwrap();
    assert_eq!(socket.error().unwrap(), i32::MIN);
    assert_eq!(
        socket
            .get_option(PacketSocketOption::RECEIVE_BUFFER)
            .unwrap(),
        Err(PacketSocketOptionError(NonZeroI32::new(-7).unwrap()))
    );
    assert_eq!(
        socket.get_option(PacketSocketOption::SEND_BUFFER).unwrap(),
        Ok(-123)
    );
    assert_eq!(
        socket
            .set_option(PacketSocketOption::NO_DELAY, -37)
            .unwrap(),
        -37
    );
    let options = AsyncSocketPacketOptions {
        packet_id: -23,
        packet_time_params: PacketTimeUpdateParams {
            srtp_auth_key: vec![0, 255, 0, 1],
            ..Default::default()
        },
        info_signaled_after_sent: PacketInfo {
            network_id: Some(0),
            included_in_feedback: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let bytes = vec![0, 255, 17, 0];
    assert_eq!(socket.send(&bytes, &options).unwrap(), -17);
    assert_eq!(
        socket.send_to(&bytes, &address(31338), &options).unwrap(),
        -17
    );
    assert_eq!(
        *fixture.observed.sends.lock().unwrap(),
        vec![(bytes.clone(), options.clone()), (bytes, options)]
    );
    assert_eq!(socket.close().unwrap(), -19);
    drop(socket);
    assert_eq!(fixture.observed.drops.load(Ordering::SeqCst), 1);
    assert_eq!(context.state(), Err(PacketSocketEventError::Retired));
    assert_eq!(
        context.notify_connected(),
        Err(PacketSocketEventError::Retired)
    );
}

#[test]
fn native_callbacks_preserve_metadata_and_self_cancel_without_recursive_delivery() {
    let fixture = Fixture::new();
    let socket = fixture.socket();
    let calls = Rc::new(Cell::new(0));
    let subscription = Rc::new(RefCell::new(None::<PacketSocketSubscription>));
    let callback_calls = calls.clone();
    let callback_subscription = subscription.clone();
    let context = socket.context().clone();
    *subscription.borrow_mut() = Some(
        socket
            .subscribe(PacketSocketEventKind::ReadyToSend, move |_| {
                callback_calls.set(callback_calls.get() + 1);
                assert_eq!(
                    context.notify_ready_to_send(),
                    Err(PacketSocketEventError::InProgress)
                );
                assert!(callback_subscription.borrow().as_ref().unwrap().cancel());
            })
            .unwrap(),
    );
    socket.notify_ready_to_send().unwrap();
    socket.notify_ready_to_send().unwrap();
    assert_eq!(calls.get(), 1);
    assert!(!subscription.borrow().as_ref().unwrap().is_active());
    assert!(!subscription.borrow().as_ref().unwrap().has_failed());
    subscription.borrow_mut().take();

    // Standalone SDK notification lists do not execute private feedback arithmetic.
    let sent = Rc::new(RefCell::new(Vec::new()));
    let captured = sent.clone();
    let sent_subscription = socket
        .subscribe(PacketSocketEventKind::Sent, move |event| {
            if let PacketSocketEvent::Sent(value) = event {
                captured.borrow_mut().push(value);
            }
        })
        .unwrap();
    let value = SentPacketInfo {
        packet_id: i64::MIN,
        send_time_ms: i64::MAX,
        info: PacketInfo {
            packet_size_bytes: usize::MAX,
            included_in_allocation: true,
            protocol: SocketPacketInfoProtocol(-77),
            network_id: Some(0),
            ..Default::default()
        },
    };
    socket.notify_sent(&value).unwrap();
    assert_eq!(*sent.borrow(), vec![value]);
    assert!(!sent_subscription.has_failed());

    let received = Rc::new(RefCell::new(Vec::new()));
    let captured = received.clone();
    let receiver = socket
        .register_received(move |packet| captured.borrow_mut().push(packet))
        .unwrap();
    assert!(matches!(
        socket.register_received(|_| {}),
        Err(PacketSocketEventError::ReceiverAlreadyRegistered)
    ));
    let mut packet = ReceivedIpPacket::new(vec![0, 255, 1, 0], address(31338));
    packet.arrival_time = Some(Timestamp::from_microseconds(-17).unwrap());
    let events = socket.events();
    let queued = packet.clone();
    std::thread::spawn(move || events.post_received(queued))
        .join()
        .unwrap()
        .unwrap();
    assert!(received.borrow().is_empty());
    assert!(!fixture.driver.pump(32).blocked);
    assert_eq!(*received.borrow(), vec![packet.clone()]);
    packet.decryption = PacketDecryptionInfo::from_native_value(3).unwrap();
    assert_eq!(
        socket.notify_received(&packet),
        Err(PacketSocketEventError::InvalidDecryption)
    );
    assert_eq!(received.borrow().len(), 1);
    drop(socket);
    assert!(!receiver.is_active());
    assert!(!sent_subscription.is_active());
}

#[test]
fn accepted_native_product_is_linear_and_outlives_its_listener() {
    let fixture = Fixture::new();
    let listener = fixture.listener();
    assert_eq!(listener.local_address().unwrap(), address(31337));
    assert_eq!(listener.state().unwrap(), ListenSocketState::BOUND);
    let order = Rc::new(RefCell::new(Vec::new()));
    let observed_order = order.clone();
    let observer = listener
        .observe_accept(move |context| {
            assert_eq!(context.state().unwrap(), PacketSocketState::BOUND);
            observed_order.borrow_mut().push("observer");
        })
        .unwrap();
    let adopted = Rc::new(RefCell::new(None));
    let accepted = adopted.clone();
    let owner_order = order.clone();
    let owner = listener
        .register_accept(move |socket| {
            owner_order.borrow_mut().push("owner");
            assert!(accepted.borrow_mut().replace(socket).is_none());
        })
        .unwrap();
    assert!(matches!(
        listener.register_accept(|_| {}),
        Err(PacketSocketEventError::AcceptOwnerAlreadyRegistered)
    ));
    let observed = fixture.observed.clone();
    let child = listener
        .create_accepted(move |_| Box::new(Socket::new(observed)))
        .unwrap();
    let ticket = listener.post_accept(child).unwrap();
    assert_eq!(ticket.status(), PacketSocketAcceptStatus::Pending);
    assert!(adopted.borrow().is_none());
    assert!(!fixture.driver.pump(32).blocked);
    assert_eq!(ticket.status(), PacketSocketAcceptStatus::Delivered);
    assert_eq!(*order.borrow(), vec!["observer", "owner"]);
    drop(listener);
    assert!(!owner.is_active());
    assert!(!observer.is_active());
    let child = adopted.borrow_mut().take().unwrap();
    let child_context = child.context().clone();
    assert_eq!(child.state().unwrap(), PacketSocketState::BOUND);
    assert_eq!(fixture.observed.drops.load(Ordering::SeqCst), 0);
    drop(child);
    assert_eq!(fixture.observed.drops.load(Ordering::SeqCst), 1);
    assert_eq!(child_context.state(), Err(PacketSocketEventError::Retired));
    assert!(!ticket.cancel());
    assert_eq!(ticket.status(), PacketSocketAcceptStatus::Delivered);
}

#[test]
fn cancelled_product_drop_during_run_pruning_retires_listener_before_delivery() {
    let fixture = Fixture::new();
    let listener = Rc::new(RefCell::new(Some(fixture.listener())));
    let context = listener.borrow().as_ref().unwrap().context().clone();
    let delivered = Rc::new(Cell::new(0));
    let calls = delivered.clone();
    let _owner = listener
        .borrow()
        .as_ref()
        .unwrap()
        .register_accept(move |_| calls.set(calls.get() + 1))
        .unwrap();
    let observed = fixture.observed.clone();
    let first = context
        .create_accepted(move |_| Box::new(Socket::new(observed)))
        .unwrap();
    let retiring_listener = listener.clone();
    let observed = fixture.observed.clone();
    let cancelled = context
        .create_accepted(move |_| {
            let mut socket = Socket::new(observed);
            socket.on_drop = Some(Box::new(move || {
                drop(retiring_listener.borrow_mut().take())
            }));
            Box::new(socket)
        })
        .unwrap();
    let first = context.post_accept(first).unwrap();
    let cancelled = context.post_accept(cancelled).unwrap();
    assert!(cancelled.cancel());
    // Running the first request prunes the second, whose destructor retires the
    // listener on that stack. The first must recheck its actual facade loan.
    assert!(!fixture.driver.pump(32).blocked);
    assert_eq!(first.status(), PacketSocketAcceptStatus::Retired);
    assert_eq!(cancelled.status(), PacketSocketAcceptStatus::Cancelled);
    assert_eq!(delivered.get(), 0);
    assert!(listener.borrow().is_none());
    assert_eq!(context.state(), Err(PacketSocketEventError::Retired));
    assert_eq!(fixture.observed.drops.load(Ordering::SeqCst), 2);
}

#[test]
fn cancelled_product_drop_during_submission_returns_the_unconsumed_socket() {
    let fixture = Fixture::new();
    let listener = Rc::new(RefCell::new(Some(fixture.listener())));
    let context = listener.borrow().as_ref().unwrap().context().clone();
    let _owner = listener
        .borrow()
        .as_ref()
        .unwrap()
        .register_accept(|_| panic!("retired listener delivered a socket"))
        .unwrap();
    let retiring_listener = listener.clone();
    let observed = fixture.observed.clone();
    let cancelled = context
        .create_accepted(move |_| {
            let mut socket = Socket::new(observed);
            socket.on_drop = Some(Box::new(move || {
                drop(retiring_listener.borrow_mut().take())
            }));
            Box::new(socket)
        })
        .unwrap();
    let observed = fixture.observed.clone();
    let second = context
        .create_accepted(move |_| Box::new(Socket::new(observed)))
        .unwrap();
    let cancelled = context.post_accept(cancelled).unwrap();
    assert!(cancelled.cancel());
    let error = context
        .notify_accept(second)
        .err()
        .expect("retired listener admitted a new socket");
    assert_eq!(error.error, PacketSocketEventError::Retired);
    assert_eq!(cancelled.status(), PacketSocketAcceptStatus::Cancelled);
    assert_eq!(fixture.observed.drops.load(Ordering::SeqCst), 1);
    drop(error.socket);
    assert_eq!(fixture.observed.drops.load(Ordering::SeqCst), 2);
    assert!(!fixture.driver.pump(32).blocked);
}

#[test]
fn rejected_creation_retires_events_without_stock_substitution() {
    let fixture = Fixture::new();
    fixture.observed.reject.store(true, Ordering::SeqCst);
    assert!(
        fixture
            .factory
            .create_udp_socket_controlled(&fixture.driver, &fixture.environment, address(0), 0, 0)
            .unwrap()
            .is_none()
    );
    let events = fixture.observed.events.lock().unwrap().take().unwrap();
    assert_eq!(
        events.post_connected(),
        Err(PacketSocketEventError::Retired)
    );
    assert_eq!(fixture.observed.drops.load(Ordering::SeqCst), 0);
    assert!(!fixture.factory.has_failed());
}
