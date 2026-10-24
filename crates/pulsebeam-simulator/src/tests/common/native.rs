use std::{
    cell::RefCell,
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    rc::{Rc, Weak},
    sync::Arc,
    time::{Duration, Instant},
};

use pulsebeam_agent_native::Error;
use pulsebeam_core::net::UdpSocket;
use pulsebeam_rtc_simulation::{
    ExternalPacket, ManagedRuntime, NetworkAddress, NetworkEndpoint, NetworkError, OutboundKind,
    PacketBridge, SimulationWorld,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use turmoil::net::{TcpStream, tcp::OwnedWriteHalf};

const PACKETS: usize = 128;
const TASKS: usize = 512;
const SOCKETS: usize = 64;

thread_local! {
    static SERVICES: RefCell<Weak<RefCell<Services>>> = const { RefCell::new(Weak::new()) };
}

struct Services {
    bridge: PacketBridge,
    sim_origin: Duration,
    native_origin: Duration,
    callback_clock: NativeCallbackClock,
    hosts: BTreeMap<IpAddr, Weak<HostState>>,
    failure: Option<String>,
    dropped_egress: u64,
}

impl Services {
    fn acquire() -> anyhow::Result<Rc<RefCell<Self>>> {
        SERVICES.with(|slot| {
            if let Some(services) = slot.borrow().upgrade() {
                return Ok(services);
            }
            let world = SimulationWorld::acquire();
            let native_origin = world.controlled.now();
            let sim_origin = turmoil::sim_elapsed()
                .ok_or_else(|| anyhow::anyhow!("native host requires a Turmoil context"))?;
            anyhow::ensure!(
                tokio::runtime::Handle::try_current().is_ok(),
                "native callback clock anchor requires the simulator Tokio context"
            );
            let callback_clock = NativeCallbackClock {
                native_origin_us: i64::try_from(native_origin.as_micros())?,
                sim_origin: Instant::now(),
            };
            let services = Rc::new(RefCell::new(Self {
                bridge: PacketBridge::new(world)?,
                sim_origin,
                native_origin,
                callback_clock,
                hosts: BTreeMap::new(),
                failure: None,
                dropped_egress: 0,
            }));
            *slot.borrow_mut() = Rc::downgrade(&services);
            Ok(services)
        })
    }

    fn fail(&mut self, error: impl std::fmt::Display) {
        self.failure.get_or_insert_with(|| error.to_string());
    }

    fn progress(&mut self, elapsed: Duration) -> anyhow::Result<bool> {
        if let Some(error) = &self.failure {
            anyhow::bail!("native virtual transport: {error}");
        }
        let target = self.native_origin + elapsed.saturating_sub(self.sim_origin);
        let world = &self.bridge.world.controlled;
        // Work posted during the last Turmoil turn runs before advancing past
        // its deadline. Native intermediate deadlines are not skipped either.
        if world
            .next_deadline()
            .is_none_or(|deadline| deadline > world.now())
        {
            let next = world
                .next_deadline()
                .map_or(target, |deadline| deadline.min(target));
            if next > world.now() {
                world.advance(next - world.now())?;
            }
        }
        let pumped = world.pump(TASKS);
        anyhow::ensure!(!pumped.blocked, "native pump reentered a provider callback");
        let turn = self.bridge.poll(PACKETS)?;
        for packet in turn.outgoing {
            let source = packet.packet().source.ip();
            let host = self.hosts.get(&source).and_then(Weak::upgrade);
            let Some(host) = host else {
                anyhow::bail!("native packet has no owning virtual host: {source}");
            };
            if host.outgoing.try_send(packet).is_err() {
                // The owned decision rejects itself. This is transport queue
                // loss, not successful native delivery or source admission.
                self.dropped_egress = self.dropped_egress.saturating_add(1);
            }
        }
        Ok(pumped.ready || turn.budget_exhausted || world.now() < target)
    }
}

/// Both existing simulation-loop owners call this wrapper. A bounded native
/// continuation consumes ready work without advancing Turmoil time. Native
/// deadlines are visited between its ordinary fixed-duration ticks.
pub fn step(sim: &mut turmoil::Sim<'_>) -> turmoil::Result<bool> {
    let services = SERVICES.with(|slot| slot.borrow().upgrade());
    if let Some(services) = &services
        && services.borrow_mut().progress(sim.elapsed())?
    {
        return Ok(false);
    }
    let finished = sim.step()?;
    if let Some(services) = SERVICES.with(|slot| slot.borrow().upgrade()) {
        let more = services.borrow_mut().progress(sim.elapsed())?;
        return Ok(finished && !more);
    }
    Ok(finished)
}

#[derive(Clone)]
pub struct NativeHost(Rc<HostState>);

/// Same controlled native clock used by RTCStats, not Rust's wall clock or a
/// guessed capture-time offset. Used only to bound real native observations.
pub fn now_us() -> anyhow::Result<i64> {
    let services = Services::acquire()?;
    let now = services.borrow().bridge.world.controlled.now();
    Ok(i64::try_from(now.as_micros())?)
}

/// Exact anchor pair from the existing native/simulator scheduling relation.
/// Callback projection is pure arithmetic, Send, and never reenters the SDK or
/// depends on Tokio context being present on a native callback sequence.
#[derive(Clone, Copy, Debug)]
pub struct NativeCallbackClock {
    native_origin_us: i64,
    sim_origin: Instant,
}
impl NativeCallbackClock {
    pub fn project(self, native_observed_at_us: i64) -> anyhow::Result<Instant> {
        let delta = native_observed_at_us
            .checked_sub(self.native_origin_us)
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| anyhow::anyhow!("native callback precedes its actual clock anchor"))?;
        self.sim_origin
            .checked_add(Duration::from_micros(delta))
            .ok_or_else(|| anyhow::anyhow!("native callback clock projection overflow"))
    }
}

pub fn callback_clock() -> anyhow::Result<NativeCallbackClock> {
    Ok(Services::acquire()?.borrow().callback_clock)
}

struct HostState {
    interface: Rc<NetworkEndpoint>,
    io: Rc<Transport>,
    outgoing: mpsc::Sender<ExternalPacket>,
    task: JoinHandle<()>,
}

impl Drop for HostState {
    fn drop(&mut self) {
        self.task.abort();
        self.io.flows.borrow_mut().clear();
    }
}

impl NativeHost {
    pub fn bind(ip: IpAddr) -> anyhow::Result<Self> {
        let services = Services::acquire()?;
        if let Some(host) = services.borrow().hosts.get(&ip).and_then(Weak::upgrade) {
            return Ok(Self(host));
        }
        let interface = services.borrow_mut().bridge.endpoint(ip)?;
        let (outgoing, incoming) = mpsc::channel(PACKETS);
        let io = Rc::new(Transport {
            services: services.clone(),
            flows: RefCell::new(Flows::default()),
            active: RefCell::new(Weak::new()),
        });
        let transport = io.clone();
        let task = tokio::task::spawn_local(async move {
            if let Err(error) = transport.run(incoming).await {
                transport.services.borrow_mut().fail(error);
            }
        });
        let host = Rc::new(HostState {
            interface,
            io,
            outgoing,
            task,
        });
        services.borrow_mut().hosts.insert(ip, Rc::downgrade(&host));
        Ok(Self(host))
    }

    pub fn h264_fixture(
        &self,
        temporal_layers: u8,
    ) -> anyhow::Result<Vec<pulsebeam_rtc_simulation::NativeH264AccessUnit>> {
        let services = self.0.io.services.borrow();
        pulsebeam_rtc_simulation::stock_h264_fixture(&services.bridge.world, temporal_layers, 48)
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }

    pub fn runtime(&self) -> Result<(ManagedRuntime, Rc<RuntimeLease>), Error> {
        let existing = self.0.io.active.borrow().upgrade();
        let owner = existing.unwrap_or_else(|| {
            let owner = Rc::new(RuntimeLease {
                host: self.0.clone(),
                token: CancellationToken::new(),
            });
            *self.0.io.active.borrow_mut() = Rc::downgrade(&owner);
            owner
        });
        let runtime = self
            .0
            .io
            .services
            .borrow()
            .bridge
            .world
            .controlled
            .runtime()
            .map_err(|error| Error::Rtc(error.to_string()))?
            .with_endpoint(&self.0.interface)
            .map_err(|error| Error::Rtc(error.to_string()))?;
        Ok((runtime, owner))
    }
}

/// Shared host transport survives overlapping graphs, but is cancelled once
/// their last actual native graph is gone. It does not retain generation history.
pub struct RuntimeLease {
    host: Rc<HostState>,
    token: CancellationToken,
}
impl Drop for RuntimeLease {
    fn drop(&mut self) {
        self.token.cancel();
        self.host.io.flows.borrow_mut().clear();
    }
}

type TcpFlow = (Arc<Mutex<OwnedWriteHalf>>, ReceiverTask);

#[derive(Default)]
struct Flows {
    udp: BTreeMap<NetworkAddress, (Arc<UdpSocket>, ReceiverTask)>,
    tcp: BTreeMap<(NetworkAddress, NetworkAddress), TcpFlow>,
    retiring: Vec<ReceiverTask>,
}
impl Flows {
    fn clear(&mut self) {
        // Abort requests cancellation, but the receiver future still owns its
        // socket until task destruction is acknowledged by joining it.
        self.retiring.extend(
            std::mem::take(&mut self.udp)
                .into_values()
                .map(|(_, task)| task)
                .chain(
                    std::mem::take(&mut self.tcp)
                        .into_values()
                        .map(|(_, task)| task),
                )
                .map(ReceiverTask::retire),
        );
        // Each map has at most SOCKETS entries. The transport joins this batch
        // before admitting any replacement flow, so no generation history grows.
        debug_assert!(self.retiring.len() <= 2 * SOCKETS);
    }
}
struct ReceiverTask(JoinHandle<()>);
impl ReceiverTask {
    fn retire(self) -> Self {
        self.0.abort();
        self
    }
    async fn join(mut self) -> anyhow::Result<()> {
        match (&mut self.0).await {
            Ok(()) => Ok(()),
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
impl Drop for ReceiverTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Transport {
    services: Rc<RefCell<Services>>,
    flows: RefCell<Flows>,
    active: RefCell<Weak<RuntimeLease>>,
}

fn socket_addr(address: NetworkAddress) -> SocketAddr {
    SocketAddr::new(address.ip(), address.port())
}
// Turmoil binds a virtual host interface via its wildcard address. Its UDP
// implementation supplies the host IP on egress, preserving the native tuple.
fn udp_bind_addr(address: NetworkAddress) -> SocketAddr {
    if address.ip().is_loopback() {
        return socket_addr(address);
    }
    let ip = match address.ip() {
        IpAddr::V4(_) => std::net::Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => std::net::Ipv6Addr::UNSPECIFIED.into(),
    };
    SocketAddr::new(ip, address.port())
}

fn network_addr(address: SocketAddr) -> Result<NetworkAddress, NetworkError> {
    NetworkAddress::new(address.ip(), address.port())
}
fn retired(error: NetworkError) -> bool {
    matches!(
        error,
        NetworkError::SocketClosed
            | NetworkError::EndpointClosed
            | NetworkError::DestinationUnavailable
            | NetworkError::PacketNotFound
    )
}

impl Transport {
    async fn run(&self, mut incoming: mpsc::Receiver<ExternalPacket>) -> anyhow::Result<()> {
        while let Some(packet) = incoming.recv().await {
            let retiring = std::mem::take(&mut self.flows.borrow_mut().retiring);
            for task in retiring {
                task.join().await?;
            }
            // This barrier is outside the per-lease cancellation select: a
            // replacement lease must not interrupt acknowledgement and lose it.
            let token = self
                .active
                .borrow()
                .upgrade()
                .map(|lease| lease.token.clone());
            let Some(token) = token else { continue };
            tokio::select! {
                _ = token.cancelled() => {},
                result = self.send(packet, token.clone()) => result?,
            }
        }
        Ok(())
    }

    async fn send(&self, packet: ExternalPacket, token: CancellationToken) -> anyhow::Result<()> {
        let source = packet.packet().source;
        let destination = packet.packet().destination;
        match packet.packet().kind {
            OutboundKind::Udp => {
                let socket = self
                    .flows
                    .borrow()
                    .udp
                    .get(&source)
                    .map(|(socket, _)| socket.clone());
                let socket = match socket {
                    Some(socket) => socket,
                    None => {
                        anyhow::ensure!(
                            self.flows.borrow().udp.len() < SOCKETS,
                            "native host UDP socket capacity"
                        );
                        let socket =
                            Arc::new(UdpSocket::bind(udp_bind_addr(source)).await.map_err(
                                |error| {
                                    anyhow::anyhow!(
                                        "UDP bind source={} lease_cancelled={}: {error:?}",
                                        udp_bind_addr(source),
                                        token.is_cancelled()
                                    )
                                },
                            )?);
                        let reader = socket.clone();
                        let receiver_token = token.clone();
                        let services = self.services.clone();
                        let task = tokio::task::spawn_local(async move {
                            let mut data = vec![0; 65_535];
                            loop {
                                let received = tokio::select! {
                                    _ = receiver_token.cancelled() => break,
                                    received = reader.recv_from(&mut data) => received,
                                };
                                let (len, remote) = match received {
                                    Ok(received) => received,
                                    Err(error) => {
                                        services.borrow_mut().fail(format!(
                                            "UDP recv_from source={} lease_cancelled={}: {error}",
                                            udp_bind_addr(source),
                                            receiver_token.is_cancelled()
                                        ));
                                        break;
                                    }
                                };
                                if receiver_token.is_cancelled() {
                                    break;
                                }
                                let result = network_addr(remote)
                                    .map_err(anyhow::Error::from)
                                    .and_then(|remote| {
                                        services
                                            .borrow_mut()
                                            .bridge
                                            .receive_udp(remote, source, data[..len].to_vec())
                                            .map_err(Into::into)
                                    });
                                if let Err(error) = result {
                                    services.borrow_mut().fail(error);
                                    break;
                                }
                            }
                        });
                        self.flows
                            .borrow_mut()
                            .udp
                            .insert(source, (socket.clone(), ReceiverTask(task)));
                        socket
                    }
                };
                socket
                    .send_to(&packet.packet().payload, socket_addr(destination))
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "UDP send_to source={} destination={} lease_cancelled={}: {error}",
                            udp_bind_addr(source),
                            socket_addr(destination),
                            token.is_cancelled()
                        )
                    })?;
            }
            OutboundKind::TcpConnect => {
                anyhow::ensure!(
                    self.flows.borrow().tcp.len() < SOCKETS,
                    "native host TCP socket capacity"
                );
                // Turmoil allocates the external ephemeral TCP port. This stream
                // remains bound to the original native socket tuple for ingress.
                let Ok(stream) = TcpStream::connect(socket_addr(destination)).await else {
                    // Dropping the decision reports native connect failure.
                    return Ok(());
                };
                let (mut reader, writer) = stream.into_split();
                let services = self.services.clone();
                let task = tokio::task::spawn_local(async move {
                    let mut data = vec![0; 16_384];
                    loop {
                        let received = tokio::select! {
                            _ = token.cancelled() => break,
                            received = reader.read(&mut data) => received,
                        };
                        let len = match received {
                            Ok(0) | Err(_) => break,
                            Ok(len) => len,
                        };
                        if token.is_cancelled() {
                            break;
                        }
                        let result =
                            services
                                .borrow()
                                .bridge
                                .receive_tcp(destination, source, &data[..len]);
                        if let Err(error) = result {
                            if !retired(error) {
                                services.borrow_mut().fail(error);
                            }
                            break;
                        }
                    }
                });
                self.flows.borrow_mut().tcp.insert(
                    (source, destination),
                    (Arc::new(Mutex::new(writer)), ReceiverTask(task)),
                );
            }
            OutboundKind::TcpData => {
                let writer = self
                    .flows
                    .borrow()
                    .tcp
                    .get(&(source, destination))
                    .map(|(writer, _)| writer.clone());
                let Some(writer) = writer else { return Ok(()) };
                if writer
                    .lock()
                    .await
                    .write_all(&packet.packet().payload)
                    .await
                    .is_err()
                {
                    self.flows.borrow_mut().tcp.remove(&(source, destination));
                    return Ok(());
                }
            }
        }
        if let Err(error) = packet.accept() {
            if !retired(error) {
                return Err(error.into());
            }
            self.flows.borrow_mut().tcp.remove(&(source, destination));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_callback_clock_uses_the_explicit_anchor_without_runtime_context() {
        let origin = Instant::now();
        let clock = NativeCallbackClock {
            native_origin_us: 7_000_000,
            sim_origin: origin,
        };
        assert_eq!(clock.project(7_000_000).unwrap(), origin);
        assert_eq!(
            clock.project(7_123_456).unwrap(),
            origin + Duration::from_micros(123_456)
        );
        assert!(clock.project(6_999_999).is_err());
        assert!(clock.project(i64::MIN).is_err());
    }

    #[test]
    fn retired_udp_receivers_release_the_socket_before_tuple_reuse() {
        let mut sim = turmoil::Builder::new().build();
        sim.client("native-host", async {
            let source = NetworkAddress::new("192.168.0.2".parse().unwrap(), 49152).unwrap();
            let mut flows = Flows::default();
            for _ in 0..3 {
                let socket = Arc::new(UdpSocket::bind(udp_bind_addr(source)).await.unwrap());
                let weak = Arc::downgrade(&socket);
                let receiver_socket = socket.clone();
                let receiver = ReceiverTask(tokio::task::spawn_local(async move {
                    let mut data = [0; 1];
                    let _ = receiver_socket.recv_from(&mut data).await;
                }));
                flows.udp.insert(source, (socket, receiver));
                tokio::task::yield_now().await;
                flows.clear();
                assert!(flows.udp.is_empty());
                assert_eq!(flows.retiring.len(), 1);
                assert!(
                    weak.upgrade().is_some(),
                    "abort is not destruction acknowledgement"
                );
                for task in std::mem::take(&mut flows.retiring) {
                    task.join().await.unwrap();
                }
                assert!(
                    weak.upgrade().is_none(),
                    "joined receiver must release its socket"
                );
                assert!(flows.retiring.is_empty());
                // The following iteration rebinds this exact host/port, without
                // a retry, wall-clock wait or virtual-time advance.
            }
            Ok(())
        });
        sim.run().unwrap();
    }

    #[test]
    fn native_udp_uses_host_wildcard_and_preserves_port() {
        for ip in ["192.168.1.2", "2001:db8::2", "127.0.0.1", "::1"] {
            let source = NetworkAddress::new(ip.parse().unwrap(), 49152).unwrap();
            let bind = udp_bind_addr(source);
            assert_eq!(bind.port(), source.port());
            assert_eq!(bind.is_ipv4(), source.ip().is_ipv4());
            if source.ip().is_loopback() {
                assert_eq!(bind.ip(), source.ip());
            } else {
                assert!(bind.ip().is_unspecified());
            }
        }
    }
}
