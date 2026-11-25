//! Explicitly provisioned WAN socket owner. It only accepts packets from the
//! fixed peer path and emits shard frames after exact local installation receipts.

use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    num::{NonZeroU16, NonZeroU32, NonZeroUsize},
};

use pulsebeam_core::fabric::wan::Window;
use pulsebeam_link::{
    LinkTime, LinkTx, NetworkId, ReceiveTime,
    wan::{
        self, Binding, Direction, Lease, Purpose, SocketOutcome, Staged, Submission, WindowSpec,
    },
};
use pulsebeam_runtime::time::Instant;
use pulsebeam_udp::{BindOptions, SendDatagram, SendStop, UdpSocket};

use crate::{
    route::fabric::{
        LiveNetworkChild,
        local::{MAX_BRANCHES, OwnedPacket},
        network_repair::{MediaRepairChild, RepairChild},
    },
    shard::worker::ShardFrame,
};

use super::{delivery_tag, fabric_codec::FabricWanCodec};

mod egress;
mod installation;
mod service;
#[cfg(test)]
pub(crate) use egress::SendFixture;
pub(crate) use egress::{ParentSend, PendingTx, SendParent, WanTx};
pub(crate) use installation::{ChildInstall, Commands, ReceiveCommand};
use installation::{Command, Pending};
pub(crate) use service::{MAX_EPOCH_WINDOWS, Provisioner, Service};

const RECEIVE_BUDGET: usize = 32;

#[cfg(all(test, target_os = "linux", not(feature = "sim")))]
mod tests;
#[cfg(all(test, target_os = "linux", not(feature = "sim")))]
pub(crate) use tests::live_send_fixture;

#[derive(Debug)]
pub(crate) enum Error {
    Io(io::Error),
    Link(wan::Error),
    Binding,
    Capacity,
    ControllerClosed,
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<wan::Error> for Error {
    fn from(error: wan::Error) -> Self {
        Self::Link(error)
    }
}

struct Destination {
    pub child: LiveNetworkChild,
    pub tx: LinkTx<ShardFrame>,
}

pub(crate) use activity::ReceiveActivity;

#[allow(
    clippy::disallowed_types,
    reason = "WAN socket publishes the first complete delivery once to its controller"
)]
mod activity {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[derive(Clone, Default)]
    pub(crate) struct ReceiveActivity(Arc<AtomicBool>);

    impl ReceiveActivity {
        pub(crate) fn observed(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }

        pub(super) fn mark_observed(&self) {
            self.0.store(true, Ordering::Release);
        }
    }
}

struct LiveReceive {
    window: Staged<Window>,
    deadline: Option<wan::LiveDeadline>,
    activity: Option<ReceiveActivity>,
    timing: Option<crate::route::fabric::WindowTiming>,
    destinations: Vec<Destination>,
}

impl LiveReceive {
    fn expires(&self) -> std::time::Instant {
        self.deadline
            .as_ref()
            .map_or_else(|| self.window.spec().expires, wan::LiveDeadline::expires)
    }
}

impl Drop for LiveReceive {
    fn drop(&mut self) {
        fence_receive(self);
    }
}

struct RepairReceive {
    window: Staged<Window>,
    parent: LiveNetworkChild,
    ingress: pulsebeam_link::FabricIngress,
    tx: LinkTx<ShardFrame>,
}

struct RepairReceiveSpec {
    parent: LiveNetworkChild,
    ingress: pulsebeam_link::FabricIngress,
    window: Window,
    position: u64,
    expires: Instant,
}

impl Drop for RepairReceive {
    fn drop(&mut self) {
        fence_destination(&self.tx);
    }
}

pub(crate) struct WanHost {
    id: NetworkId,
    origin: Instant,
    socket: UdpSocket,
    driver: wan::Driver<FabricWanCodec, Window>,
    receives: BTreeMap<NonZeroU32, LiveReceive>,
    repairs: BTreeMap<NonZeroU32, RepairReceive>,
    pending: BTreeMap<NonZeroU32, Pending>,
    pending_capacity: usize,
    commands: Option<pulsebeam_queue::Receiver<Command>>,
    egress: egress::Egress,
    lease: Option<Lease>,
}

pub(crate) async fn bind_socket(address: SocketAddr) -> io::Result<UdpSocket> {
    let mut options = BindOptions::new(address);
    options.max_datagram_bytes = wan::MAX_DATAGRAM_BYTES;
    options.receive_ecn = true;
    options.nonfragmenting_max_datagram_bytes = u16::try_from(wan::MAX_DATAGRAM_BYTES)
        .ok()
        .and_then(NonZeroU16::new);
    pulsebeam_udp::bind(options).await?.activate()
}

impl WanHost {
    #[cfg(all(test, target_os = "linux", not(feature = "sim")))]
    fn new(
        socket: UdpSocket,
        id: NetworkId,
        config: wan::Config<'_>,
        origin: Instant,
    ) -> Result<(Self, pulsebeam_link::PeerControl), Error> {
        if socket
            .nonfragmenting_max_datagram_bytes()
            .map(|cap| usize::from(cap.get()))
            != Some(wan::MAX_DATAGRAM_BYTES)
            || socket.local_addr().ip().is_unspecified()
            || config.binding.epoch.get() != id.epoch()
        {
            return Err(Error::Binding);
        }
        let pending_capacity = config.limits.active_windows;
        let (driver, control) =
            wan::Driver::with_control(config, FabricWanCodec, origin.into_std())?;
        Ok((
            Self::attach(socket, id, origin, driver, pending_capacity),
            control,
        ))
    }

    fn attach(
        socket: UdpSocket,
        id: NetworkId,
        origin: Instant,
        driver: wan::Driver<FabricWanCodec, Window>,
        pending_capacity: usize,
    ) -> Self {
        Self {
            id,
            origin,
            socket,
            driver,
            receives: BTreeMap::new(),
            repairs: BTreeMap::new(),
            pending: BTreeMap::new(),
            pending_capacity,
            commands: None,
            egress: egress::Egress::default(),
            lease: None,
        }
    }

    pub(crate) fn binding(&self) -> Binding {
        self.driver.binding()
    }

    fn stage_live_receive(
        &mut self,
        now: Instant,
        expires: Instant,
        deadline: Option<wan::LiveDeadline>,
        timing: Option<crate::route::fabric::WindowTiming>,
        destinations: Vec<Destination>,
        activity: Option<ReceiveActivity>,
    ) -> Result<LiveReceive, Error> {
        let first = destinations.first().ok_or(Error::Binding)?.child;
        let window = first.window.ok_or(Error::Binding)?;
        if window.profile() != first.emission.profile()
            || window.emission() != first.emission.wire_profile().1
            || destinations.len() > MAX_BRANCHES
            || destinations.iter().enumerate().any(|(i, destination)| {
                let child = destination.child;
                child.ingress != first.ingress
                    || child.ingress.association != self.id
                    || child.ingress.purpose != pulsebeam_link::FabricPurpose::Live
                    || child.ingress.key.to_bytes() != window.ssrc().get().to_be_bytes()
                    || child.window != Some(window)
                    || child.incarnation != first.incarnation
                    || child.publication != first.publication
                    || child.emission != first.emission
                    || child.major != first.major
                    || child.revision != first.revision
                    || child.sequence_anchor != first.sequence_anchor
                    || destination.tx.local_delivery_tag() != Some(delivery_tag(child.destination))
                    || !destination.tx.is_live()
                    || self.receive_tx_in_use(&destination.tx)
                    || destinations
                        .iter()
                        .take(i)
                        .any(|earlier| earlier.child.destination == child.destination)
            })
        {
            return Err(Error::Binding);
        }
        let spec = WindowSpec {
            authority: first.incarnation,
            direction: Direction::Receive,
            start: window.start(),
            ssrc: window.ssrc(),
            purpose: Purpose::Live,
            expires: expires.into_std(),
        };
        let staged = self.driver.stage_with_deadline(
            now.into_std(),
            self.binding(),
            spec,
            window,
            deadline.clone(),
        )?;
        Ok(LiveReceive {
            window: staged,
            deadline,
            activity,
            timing: timing.filter(|point| point.position == first.sequence_anchor),
            destinations,
        })
    }

    fn stage_repair_receive(
        &mut self,
        now: Instant,
        parent: LiveNetworkChild,
        child: RepairChild,
        tx: LinkTx<ShardFrame>,
    ) -> Result<RepairReceive, Error> {
        let window = child.window.ok_or(Error::Binding)?;
        if !child.matches_parent(parent)
            || window.profile() != pulsebeam_core::fabric::Profile::Data
        {
            return Err(Error::Binding);
        }
        self.stage_exact_repair_receive(
            now,
            RepairReceiveSpec {
                parent,
                ingress: child.route.ingress,
                window,
                position: child.demand.sequence,
                expires: child.demand.expires,
            },
            tx,
        )
    }

    fn stage_media_repair_receive(
        &mut self,
        now: Instant,
        child: MediaRepairChild,
        tx: LinkTx<ShardFrame>,
    ) -> Result<RepairReceive, Error> {
        if !child.valid() {
            return Err(Error::Binding);
        }
        self.stage_exact_repair_receive(
            now,
            RepairReceiveSpec {
                parent: child.parent,
                ingress: child.ingress(),
                window: child.window,
                position: child.position,
                expires: child.expires,
            },
            tx,
        )
    }

    fn stage_exact_repair_receive(
        &mut self,
        now: Instant,
        route: RepairReceiveSpec,
        tx: LinkTx<ShardFrame>,
    ) -> Result<RepairReceive, Error> {
        self.fence_stale(now);
        let RepairReceiveSpec {
            parent,
            ingress,
            window,
            position,
            expires,
        } = route;
        if !tx.is_live()
            || tx.local_delivery_tag() != Some(delivery_tag(parent.destination))
            || !Self::has_live_parent(&self.receives, parent, &tx)
            || self.receive_tx_in_use(&tx)
            || parent
                .window
                .and_then(|window| self.receives.get(&window.ssrc()))
                .is_none_or(|live| expires.into_std() > live.expires())
        {
            return Err(Error::Binding);
        }
        let spec = WindowSpec {
            authority: parent.incarnation,
            direction: Direction::Receive,
            start: window.start(),
            ssrc: window.ssrc(),
            purpose: Purpose::Repair { position },
            expires: expires.into_std(),
        };
        let staged = self
            .driver
            .stage(now.into_std(), self.binding(), spec, window)?;
        Ok(RepairReceive {
            window: staged,
            parent,
            ingress,
            tx,
        })
    }

    fn has_live_parent(
        receives: &BTreeMap<NonZeroU32, LiveReceive>,
        parent: LiveNetworkChild,
        repair_tx: &LinkTx<ShardFrame>,
    ) -> bool {
        parent
            .window
            .and_then(|window| receives.get(&window.ssrc()))
            .is_some_and(|route| {
                route.destinations.iter().any(|destination| {
                    destination.child == parent
                        && destination.tx.is_live()
                        && &destination.tx != repair_tx
                })
            })
    }

    #[cfg(test)]
    pub(crate) fn stage_send(
        &mut self,
        now: Instant,
        window: Window,
        spec: WindowSpec,
    ) -> Result<Staged<Window>, Error> {
        self.stage_send_with_deadline(now, window, spec, None)
    }

    fn stage_send_with_deadline(
        &mut self,
        now: Instant,
        window: Window,
        spec: WindowSpec,
        deadline: Option<wan::LiveDeadline>,
    ) -> Result<Staged<Window>, Error> {
        if spec.direction != Direction::Send {
            return Err(Error::Binding);
        }
        self.driver
            .stage_with_deadline(now.into_std(), self.binding(), spec, window, deadline)
            .map_err(Error::from)
    }

    pub(crate) fn activate_send(
        &mut self,
        now: Instant,
        staged: Staged<Window>,
        ack: Window,
        spec: WindowSpec,
    ) -> Result<(), Error> {
        if spec.direction != Direction::Send {
            return Err(Error::Binding);
        }
        self.driver
            .activate(now.into_std(), staged, ack, spec)
            .map_err(Error::from)
    }

    pub(crate) fn submit(&mut self, now: Instant, prepared: wan::Prepared) -> Submission {
        let socket = &mut self.socket;
        let ect0 = socket.supports_ect0() && self.driver.ecn_marking_allowed();
        let result = self
            .driver
            .submit(now.into_std(), prepared, &mut |destination, bytes| {
                if ect0 {
                    let report = socket.try_send_ect0(destination, bytes);
                    socket_outcome(report, bytes.len(), true)
                } else {
                    send(socket, destination, bytes)
                }
            });
        self.sync_lease(now);
        result
    }

    pub(crate) fn prepare(
        &mut self,
        now: Instant,
        staged: Staged<Window>,
        position: u64,
        packet: &OwnedPacket,
    ) -> Result<wan::Prepared, Error> {
        self.driver
            .prepare(now.into_std(), staged, position, packet)
            .map_err(Error::from)
    }

    pub(crate) fn poll(&mut self, now: Instant) -> Result<(), Error> {
        let socket = &mut self.socket;
        let result = self.driver.tick(now.into_std(), &mut |destination, bytes| {
            send(socket, destination, bytes)
        });
        self.fence_stale(now);
        result?;
        for _ in 0..RECEIVE_BUDGET {
            let Some(receiver) = &mut self.commands else {
                break;
            };
            match receiver.try_recv() {
                Ok(Command::Receive(command)) => self.install(now, command),
                Ok(Command::Send(install)) => self.install_send(now, *install),
                Err(pulsebeam_queue::TryRecvError::Empty) => break,
                Err(pulsebeam_queue::TryRecvError::Closed) => return Err(Error::ControllerClosed),
            }
        }
        self.advance_installations(now);
        let binding = self.binding();
        let local = self.socket.local_addr();
        let input = match self.socket.try_recv_batch() {
            Ok(batch) => batch
                .iter()
                .filter(|packet| packet.source == binding.peer && packet.destination == local)
                .take(RECEIVE_BUDGET)
                .map(|packet| {
                    (
                        Box::<[u8]>::from(packet.payload),
                        ReceiveTime {
                            monotonic: LinkTime::from_nanos(
                                u64::try_from(
                                    packet
                                        .received_at
                                        .monotonic
                                        .saturating_duration_since(self.origin)
                                        .as_nanos(),
                                )
                                .unwrap_or(u64::MAX),
                            ),
                            wall: Some(packet.received_at.wall),
                        },
                        packet.ecn.map(|ecn| match ecn {
                            pulsebeam_udp::Ecn::NotEct => wan::Ecn::NotEct,
                            pulsebeam_udp::Ecn::Ect1 => wan::Ecn::Ect1,
                            pulsebeam_udp::Ecn::Ect0 => wan::Ecn::Ect0,
                            pulsebeam_udp::Ecn::Ce => wan::Ecn::Ce,
                        }),
                    )
                })
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        for (bytes, observed, ecn) in input {
            #[cfg(feature = "sim")]
            let Some(bytes) = self.filter_simulated_loss(local, binding.peer, bytes, ecn) else {
                continue;
            };
            self.receive_datagram(now, binding, &bytes, observed, ecn)?;
        }
        #[cfg(feature = "sim")]
        if crate::sim_metrics::wan_repair::active(binding.peer, local)
            && let Some((bytes, ecn)) = crate::sim_metrics::wan_repair::release_original(self.id)
        {
            let elapsed = now.saturating_duration_since(self.origin);
            let observed = ReceiveTime {
                monotonic: LinkTime::from_nanos(
                    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
                ),
                wall: None,
            };
            self.receive_datagram(now, binding, &bytes, observed, ecn)?;
        }
        self.drain_media(now);
        Ok(())
    }

    fn receive_datagram(
        &mut self,
        now: Instant,
        binding: Binding,
        bytes: &[u8],
        observed: ReceiveTime,
        ecn: Option<wan::Ecn>,
    ) -> Result<(), Error> {
        let socket = &mut self.socket;
        let received = self.driver.receive_with_ecn(
            now.into_std(),
            binding,
            bytes,
            ecn,
            &mut |destination, payload| send(socket, destination, payload),
        );
        self.sync_lease(now);
        match received {
            Ok(Some(image)) => self.deliver(image, observed),
            Err(error) if self.driver.is_closed() => return Err(error.into()),
            _ => {}
        }
        Ok(())
    }

    #[cfg(feature = "sim")]
    fn filter_simulated_loss(
        &self,
        local: SocketAddr,
        peer: SocketAddr,
        bytes: Box<[u8]>,
        ecn: Option<wan::Ecn>,
    ) -> Option<Box<[u8]>> {
        use crate::sim_metrics::{self, wan_repair};
        use pulsebeam_core::fabric::Profile;

        let edge = (peer.ip(), local.ip());
        let media_probe = wan_repair::active(peer, local);
        if !media_probe && !sim_metrics::wan_data_loss_armed(edge) {
            return Some(bytes);
        }
        let [0x80 | 0x90, pt, hi, lo, t0, t1, t2, t3, a, b, c, d, ..] = bytes.as_ref() else {
            return Some(bytes);
        };
        if !(96..=103).contains(&(pt & 0x7f)) {
            return Some(bytes);
        }
        let Some(label) = NonZeroU32::new(u32::from_be_bytes([*a, *b, *c, *d])) else {
            return Some(bytes);
        };
        let route = self
            .receives
            .get(&label)
            .map(|live| (live.window, false))
            .or_else(|| self.repairs.get(&label).map(|repair| (repair.window, true)));
        let Some((route, repair)) = route else {
            return Some(bytes);
        };
        let window = route.descriptor();
        let Ok(position) = window.position(u16::from_be_bytes([*hi, *lo])) else {
            return Some(bytes);
        };
        match window.profile() {
            Profile::Opus if media_probe => {
                let timestamp = u32::from_be_bytes([*t0, *t1, *t2, *t3]);
                wan_repair::filter(self.id, bytes, ecn, window, position, timestamp, repair)
            }
            Profile::Data if !repair && sim_metrics::drop_wan_live_data(edge, position) => None,
            _ => Some(bytes),
        }
    }

    async fn drive(
        &mut self,
        shutdown: &pulsebeam_runtime::sync::CancellationToken,
    ) -> Result<(), Error> {
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            match self.poll(Instant::now()) {
                Ok(()) => {}
                Err(Error::ControllerClosed) => return Ok(()),
                Err(error) => return Err(error),
            }
            pulsebeam_runtime::select! {
                _ = shutdown.cancelled() => return Ok(()),
                ready = self.ready() => ready?,
            }
            pulsebeam_runtime::rt::yield_now().await;
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.driver
            .next_deadline()
            .into_iter()
            .chain(self.pending.values().map(Pending::expires))
            .chain(self.egress.next_deadline())
            .chain(self.receives.values().map(LiveReceive::expires))
            .chain(
                self.repairs
                    .values()
                    .map(|route| route.window.spec().expires),
            )
            .min()
            .map(Instant::from_std)
    }

    async fn ready(&mut self) -> Result<(), Error> {
        let deadline = self.next_deadline();
        let media_blocked = self.egress.has_blocked();
        let blocked = self.driver.control_write_blocked() || self.egress.socket_write_blocked();
        let timer = async {
            if let Some(deadline) = deadline {
                pulsebeam_runtime::time::sleep_until(deadline).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let writable = async {
            if blocked {
                self.socket.writable().await
            } else {
                std::future::pending().await
            }
        };
        let commands = async {
            if let Some(commands) = &mut self.commands {
                let _ = commands.readable().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let media = async {
            if !media_blocked && let Some(media) = &mut self.egress.inbox {
                if media.readable().await {
                    return;
                }
            }
            std::future::pending::<()>().await;
        };
        let receipts = std::future::poll_fn(|cx| {
            if self
                .pending
                .values_mut()
                .any(|pending| pending.poll_ready(cx).is_ready())
            {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        });
        pulsebeam_runtime::select! {
            result = self.socket.readable() => result.map_err(Error::from),
            result = writable => result.map_err(Error::from),
            _ = self.driver.control_readable() => Ok(()),
            _ = commands => Ok(()),
            _ = media => Ok(()),
            _ = receipts => Ok(()),
            _ = timer => Ok(()),
        }
    }

    fn deliver(&mut self, image: wan::Received<OwnedPacket, Window>, observed: ReceiveTime) {
        let spec = image.route.spec();
        #[cfg(feature = "sim")]
        if image.route.descriptor().profile() == pulsebeam_core::fabric::Profile::Data
            && matches!(spec.purpose, Purpose::Repair { .. })
        {
            crate::sim_metrics::record_wan_data_repair(
                (self.binding().peer.ip(), self.socket.local_addr().ip()),
                image.position,
            );
        }
        #[cfg(feature = "sim")]
        if image.route.descriptor().profile() == pulsebeam_core::fabric::Profile::Opus
            && crate::sim_metrics::wan_repair::active(self.binding().peer, self.socket.local_addr())
        {
            crate::sim_metrics::wan_repair::authenticated(
                self.id,
                image.route.descriptor(),
                image.position,
                &image.packet,
            );
        }
        let expiry = Instant::from_std(image.expires).saturating_duration_since(self.origin);
        let expires = LinkTime::from_nanos(u64::try_from(expiry.as_nanos()).unwrap_or(u64::MAX));
        let handoff = |tx: &LinkTx<ShardFrame>, ingress, image| {
            tx.try_send_received(
                ShardFrame::FabricWanImage { ingress, image },
                observed,
                expires,
            )
            .is_ok()
        };
        match spec.purpose {
            Purpose::Live => {
                #[cfg(feature = "sim")]
                let peer_ip = self.binding().peer.ip();
                let Some(route) = self
                    .receives
                    .get_mut(&spec.ssrc)
                    .filter(|route| route.window == image.route)
                else {
                    return;
                };
                #[cfg(feature = "sim")]
                if image.route.descriptor().profile() == pulsebeam_core::fabric::Profile::Data {
                    crate::sim_metrics::record_wan_live_data(
                        peer_ip,
                        self.socket.local_addr().ip(),
                        image.position,
                        image.packet.header().timestamp,
                    );
                }
                let mut frame: crate::route::fabric::NetworkPacket = image.into();
                frame.capture = route.timing.and_then(|timing| timing.capture_for(&frame));
                let mut complete = true;
                for destination in &route.destinations {
                    complete &= handoff(&destination.tx, destination.child.ingress, frame.clone());
                }
                if complete && let Some(activity) = route.activity.take() {
                    activity.mark_observed();
                }
            }
            Purpose::Repair { position } => {
                let Some(repair) = self.repairs.get(&spec.ssrc).filter(|repair| {
                    repair.window == image.route
                        && position == image.position
                        && Self::has_live_parent(&self.receives, repair.parent, &repair.tx)
                }) else {
                    return;
                };
                handoff(&repair.tx, repair.ingress, image.into());
            }
        }
    }

    fn sync_lease(&mut self, now: Instant) {
        if self.lease != self.driver.lease(now.into_std()) {
            self.fence_stale(now);
        }
    }

    fn fence_stale(&mut self, now: Instant) {
        let lease = self.driver.lease(now.into_std());
        let before = std::mem::replace(&mut self.lease, lease);
        if before != lease {
            tracing::debug!(epoch = self.id.epoch(), endpoint = %self.binding().peer, qualified = lease.is_some(), "Fabric WAN path qualification changed");
        }
        let changed = before.is_some() && before != lease;
        self.egress.fence_stale(&mut self.driver, now, changed);
        self.receives.retain(|_, route| {
            if changed
                || now.into_std() >= route.expires()
                || route
                    .destinations
                    .iter()
                    .all(|destination| !destination.tx.is_live())
            {
                fence_receive(route);
                self.driver.retire(route.window);
                false
            } else {
                true
            }
        });
        self.repairs.retain(|_, repair| {
            if changed
                || now.into_std() >= repair.window.spec().expires
                || !repair.tx.is_live()
                || !Self::has_live_parent(&self.receives, repair.parent, &repair.tx)
            {
                fence_destination(&repair.tx);
                self.driver.retire(repair.window);
                false
            } else {
                true
            }
        });
    }
}

fn command_channel(
    id: NetworkId,
) -> (
    Commands,
    pulsebeam_queue::Receiver<Command>,
    pulsebeam_queue::Receiver<egress::Media>,
) {
    let (sender, receiver) = pulsebeam_queue::bounded(const { NonZeroUsize::new(16).unwrap() });
    let (media, inbox) = pulsebeam_queue::bounded(const { NonZeroUsize::new(256).unwrap() });
    (Commands::new(id, sender, media), receiver, inbox)
}

fn fence_receive(route: &LiveReceive) {
    for destination in &route.destinations {
        fence_destination(&destination.tx);
    }
}

fn fence_destination(tx: &LinkTx<ShardFrame>) {
    if let Some(fence) = tx.local_cutoff() {
        fence.fence();
    }
}

fn send(socket: &mut UdpSocket, destination: SocketAddr, payload: &[u8]) -> SocketOutcome {
    let report = socket.try_send_batch(&[SendDatagram {
        destination,
        payload,
        gso_segment_size: None,
    }]);
    socket_outcome(report, payload.len(), false)
}

fn socket_outcome(report: pulsebeam_udp::SendReport, bytes: usize, ect0: bool) -> SocketOutcome {
    match (report.accepted, report.stop) {
        (1, _) if ect0 => SocketOutcome::AcceptedEct0 { bytes },
        (1, _) => SocketOutcome::Accepted { bytes },
        (0, Some(SendStop::Blocked)) => SocketOutcome::WouldBlock,
        (0, Some(stop)) if stop.is_path_mtu_error() => SocketOutcome::Mtu,
        (0, Some(SendStop::Error(_))) => SocketOutcome::Failed,
        _ => SocketOutcome::Unknown,
    }
}
