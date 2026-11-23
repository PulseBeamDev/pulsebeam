//! Link's single-owner host adapter. Transport owns security, feedback and CC;
//! Link owns application queues, route fences, decoding and local fanout.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::control::{self, HostLane};
use super::network::{self, NetworkCodec, Outbound, ReceiveBinding};
use super::*;
pub use pulsebeam_transport::{Datagram, Error, SendOutcome};

#[derive(Clone, Copy, Debug, Default)]
pub struct Health {
    pub ready: bool,
    pub feedback_interval: Duration,
    pub sent: u64,
    pub received: u64,
    pub send_failed: u64,
    pub expired: u64,
    pub stale: u64,
    pub invalid: u64,
    pub local_full: u64,
    pub local_closed: u64,
    pub bytes_in_flight: u64,
    pub staging: pulsebeam_transport::Staging,
    pub pending_udp_bytes: usize,
    pub quic: Option<pulsebeam_transport::QuicStats>,
    pub closed: Option<Error>,
}

pub struct NetworkDriver<P> {
    transport: pulsebeam_transport::Transport,
    codec: NetworkCodec<P>,
    state: Shared<Fence>,
    table: Shared<Table>,
    outbound: pulsebeam_queue::Receiver<Outbound<P>>,
    commands: pulsebeam_queue::Receiver<ReceiveBinding<P>>,
    routes: HashMap<NodeRouteKey, ReceiveBinding<P>>,
    pending: Option<Outbound<P>>,
    protected_relay: Option<(BindingId, BindingId)>,
    now: LinkTime,
    health: Health,
    unknown_label: Option<NodeRouteKey>,
    control: Option<HostLane>,
    prefer_control: bool,
}

impl<P> Drop for NetworkDriver<P> {
    fn drop(&mut self) {
        self.fence_receives();
    }
}

impl<P> NetworkDriver<P> {
    fn fence_receives(&mut self) {
        self.state.store(network::CLOSED, Ordering::Release);
        if self.codec.is_fabric() {
            let fence = |binding: BindingId| {
                Table::retire(&self.table.bindings, binding.slot, binding.generation);
            };
            for route in self.routes.values() {
                fence(route.binding);
            }
            while let Ok(route) = self.commands.try_recv() {
                fence(route.binding);
            }
        }
        self.routes.clear();
    }
}

fn send_checked(
    table: &Table,
    relay: Option<(BindingId, BindingId)>,
    datagram: Datagram<'_>,
    send: &mut impl FnMut(Datagram<'_>) -> SendOutcome,
) -> SendOutcome {
    if datagram.is_fabric_application()
        && relay.is_some_and(|(binding, parent)| {
            !table.binding_live(binding) || !table.binding_live(parent)
        })
    {
        SendOutcome::Rejected
    } else {
        send(datagram)
    }
}

impl<P: Clone + Send + 'static> NetworkDriver<P> {
    pub(crate) fn new(
        peer: pulsebeam_transport::PeerConfig,
        codec: NetworkCodec<P>,
        state: Shared<Fence>,
        table: Shared<Table>,
        outbound: pulsebeam_queue::Receiver<Outbound<P>>,
        commands: pulsebeam_queue::Receiver<ReceiveBinding<P>>,
        control: Option<HostLane>,
    ) -> Result<Self, network::LinkError> {
        let transport = pulsebeam_transport::Transport::new(peer).map_err(|error| match error {
            Error::UnsupportedProfile => network::LinkError::UnsupportedProfile,
            _ => network::LinkError::Capacity,
        })?;
        Ok(Self {
            transport,
            codec,
            state,
            table,
            outbound,
            commands,
            routes: HashMap::new(),
            pending: None,
            protected_relay: None,
            now: LinkTime(0),
            health: Health::default(),
            unknown_label: None,
            control,
            prefer_control: true,
        })
    }

    /// One sampled unmatched wire label, without any inferred publication.
    pub fn take_unknown_label(&mut self) -> Option<NodeRouteKey> {
        self.unknown_label.take()
    }

    /// Both clock observations must come from the same real or virtual host.
    pub fn start(&mut self, at: LinkTime, instant: Instant) -> Result<(), Error> {
        self.ensure_live()?;
        self.transport.start(at, instant)?;
        self.now = at;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn set_test_key_limit(&mut self, limit: u64) {
        self.transport.set_test_key_limit(limit);
    }

    #[cfg(test)]
    pub(super) fn seed_test_sequence(&mut self, next_attempt: u64, highest_received: Option<u64>) {
        self.transport
            .seed_test_sequence(next_attempt, highest_received);
    }

    pub fn health(&self) -> Health {
        let transport = self.transport.health();
        Health {
            ready: transport.ready
                && (!self.codec.is_fabric() || self.transport.payload_capacity() != 0),
            feedback_interval: transport.feedback_interval,
            sent: transport.sent,
            received: transport.received,
            send_failed: self
                .health
                .send_failed
                .saturating_add(transport.send_failed),
            expired: self.health.expired.saturating_add(transport.expired),
            invalid: self.health.invalid.saturating_add(transport.invalid),
            bytes_in_flight: transport.bytes_in_flight,
            staging: transport.staging,
            pending_udp_bytes: transport.pending_udp_bytes,
            quic: transport.quic,
            closed: transport.closed,
            ..self.health
        }
    }

    pub fn set_fabric_path(&mut self, path: crate::FabricPath) -> Result<(), Error> {
        self.ensure_live()?;
        self.transport.set_fabric_path(path)?;
        self.publish_readiness();
        Ok(())
    }

    fn publish_readiness(&self) {
        let (old, new) = if self.health().ready {
            (network::NOT_READY, network::READY)
        } else {
            (network::READY, network::NOT_READY)
        };
        if self.state.load(Ordering::Relaxed) == old {
            let _ = self
                .state
                .compare_exchange(old, new, Ordering::Release, Ordering::Relaxed);
        }
    }

    pub async fn readable(&mut self) -> bool {
        if self.transport.needs_progress() {
            return true;
        }
        let media = async {
            if self.pending.is_some() {
                self.commands.readable().await
            } else {
                futures_lite::future::or(self.commands.readable(), self.outbound.readable()).await
            }
        };
        if let Some(control) = self.control.as_mut().filter(|lane| lane.pending.is_none()) {
            futures_lite::future::or(media, async {
                if control.outgoing.readable().await {
                    true
                } else {
                    std::future::pending().await
                }
            })
            .await
        } else {
            media.await
        }
    }

    pub fn next_deadline(&self) -> Option<LinkTime> {
        [
            self.transport.next_deadline(),
            self.pending.as_ref().map(|pending| pending.expires_at),
            self.control
                .as_ref()
                .and_then(|lane| lane.pending.as_ref())
                .map(|pending| pending.expires_at),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn input_ready(&self) -> bool {
        self.ensure_live().is_ok() && self.transport.input_ready()
    }

    /// On Busy, ownership remains with the host; poll before retrying input.
    pub fn receive(
        &mut self,
        source: SocketAddr,
        datagram: &[u8],
        observed: ReceiveTime,
    ) -> Result<(), Error> {
        self.ensure_live()?;
        self.transport.receive(source, datagram, observed)
    }

    /// The callback reports actual nonblocking UDP submission, not host staging.
    pub fn poll(
        &mut self,
        now: LinkTime,
        budget: usize,
        mut send: impl FnMut(Datagram<'_>) -> SendOutcome,
    ) -> Result<usize, Error> {
        let result = self.poll_inner(now, budget, &mut send);
        if result.is_err() {
            self.close();
        }
        result
    }

    fn poll_inner(
        &mut self,
        now: LinkTime,
        budget: usize,
        send: &mut impl FnMut(Datagram<'_>) -> SendOutcome,
    ) -> Result<usize, Error> {
        self.ensure_live()?;
        if now < self.now {
            return Err(Error::Time);
        }
        self.transport.poll(
            now,
            0,
            |datagram| send_checked(&self.table, self.protected_relay, datagram, send),
            |_| {},
        )?;
        self.now = now;
        let mut work = 0usize;
        // Install queued receive bindings before dispatching authenticated input.
        while work < budget {
            let Ok(binding) = self.commands.try_recv() else {
                break;
            };
            self.routes
                .retain(|_, route| self.table.binding_live(route.binding));
            if self.table.binding_live(binding.binding) {
                self.routes.insert(binding.key, binding);
            }
            work = work.saturating_add(1);
        }
        while work < budget {
            if self.pending.is_none() {
                self.pending = self.outbound.try_recv().ok();
            }
            if let Some(control) = self.control.as_mut()
                && control.pending.is_none()
            {
                control.pending = control.outgoing.try_recv().ok();
            }
            self.transport.set_demand(
                self.pending.is_some()
                    || self
                        .control
                        .as_ref()
                        .is_some_and(|lane| lane.pending.is_some()),
            );
            let table = &self.table;
            let routes = &self.routes;
            let codec = &self.codec;
            let health = &mut self.health;
            let unknown_label = &mut self.unknown_label;
            let control = self.control.as_ref();
            work = work.saturating_add(self.transport.poll(
                now,
                budget.saturating_sub(work),
                |datagram| send_checked(table, self.protected_relay, datagram, send),
                |application| {
                    if let Some(control) = control
                        && application.context == Some(control::CONTEXT)
                    {
                        if application.payload.len() > control::CONTROL_FRAME_BYTES {
                            health.invalid = health.invalid.saturating_add(1);
                        } else {
                            let _ = control.incoming.try_send(application.payload.into());
                        }
                        return;
                    }
                    Self::deliver(table, routes, codec, health, unknown_label, application);
                },
            )?);
            self.publish_readiness();
            if !self.transport.health().ready || self.transport.needs_progress() {
                break;
            }
            if !self.prepare_next()? {
                break;
            }
            // A drained protocol output includes one bounded application
            // preparation, including when it consumed the final work unit.
        }
        Ok(work)
    }

    #[cfg(test)]
    pub(super) fn stage_test_outbound(&mut self) -> Result<bool, Error> {
        self.pending = self.outbound.try_recv().ok();
        self.prepare_next()
    }

    fn prepare_next(&mut self) -> Result<bool, Error> {
        if (self.prefer_control || self.pending.is_none())
            && let Some(lane) = self.control.as_mut()
            && let Some(control) = lane.pending.as_ref()
        {
            if control.expires_at <= self.now {
                lane.pending = None;
                self.prefer_control = false;
                return Ok(true);
            }
            let submission = if self.codec.is_fabric() {
                self.transport
                    .try_submit_fabric_control(&control.bytes, control.expires_at)
            } else {
                self.transport
                    .try_submit(control::CONTEXT, &control.bytes, control.expires_at)
            };
            return match submission {
                Ok(()) => {
                    lane.pending = None;
                    self.prefer_control = false;
                    Ok(true)
                }
                Err(Error::Blocked | Error::NotReady | Error::PathIncapable) => Ok(false),
                Err(error) => Err(error),
            };
        }
        let Some(pending) = &self.pending else {
            return Ok(false);
        };
        self.prefer_control = true;
        if !self.table.binding_live(pending.binding)
            || pending
                .parent
                .is_some_and(|parent| !self.table.binding_live(parent))
        {
            self.pending = None;
            self.health.stale = self.health.stale.saturating_add(1);
            return Ok(true);
        }
        if pending.expires_at <= self.now {
            self.pending = None;
            self.health.expired = self.health.expired.saturating_add(1);
            return Ok(true);
        }
        match self.transport.admission() {
            Ok(()) => {}
            Err(Error::Blocked | Error::NotReady) => return Ok(false),
            Err(Error::PathIncapable) => {
                self.pending = None;
                self.health.send_failed = self.health.send_failed.saturating_add(1);
                return Ok(true);
            }
            Err(error) => return Err(error),
        }
        let mut buffer = [0u8; wire::FABRIC_IMAGE];
        let buffer = buffer
            .get_mut(..self.codec.payload_limit())
            .ok_or(Error::Codec)?;
        let length = self
            .codec
            .encode(
                &pending.packet,
                pending.address,
                pending.source_position,
                buffer,
            )
            .map_err(|_| Error::Codec)?;
        let payload = buffer.get(..length).ok_or(Error::Codec)?;
        let result = if self.codec.is_fabric() {
            self.transport
                .try_submit_fabric(payload, pending.expires_at)
        } else {
            self.transport
                .try_submit(pending.address.to_bytes(), payload, pending.expires_at)
        };
        match result {
            Ok(()) => {
                self.protected_relay = pending.parent.map(|parent| (pending.binding, parent));
                self.pending = None;
                Ok(true)
            }
            Err(Error::Blocked | Error::NotReady) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn deliver(
        table: &Table,
        routes: &HashMap<NodeRouteKey, ReceiveBinding<P>>,
        codec: &NetworkCodec<P>,
        health: &mut Health,
        unknown_label: &mut Option<NodeRouteKey>,
        application: pulsebeam_transport::Application<'_>,
    ) {
        let Ok(label) = codec.route_key(application.context, application.payload) else {
            health.invalid = health.invalid.saturating_add(1);
            return;
        };
        let Some(binding) = routes.get(&label) else {
            health.stale = health.stale.saturating_add(1);
            unknown_label.get_or_insert(label);
            return;
        };
        if !table.binding_live(binding.binding)
            || binding
                .fabric
                .as_ref()
                .is_some_and(|route| !route.is_active())
        {
            health.stale = health.stale.saturating_add(1);
            return;
        }
        let Ok(packet) = codec.decode(application.payload) else {
            health.invalid = health.invalid.saturating_add(1);
            return;
        };
        let fabric = if let Some(policy) = &binding.fabric {
            if !(policy.admits)(&packet) {
                health.invalid = health.invalid.saturating_add(1);
                return;
            }
            Some(FabricIngress {
                association: policy.association,
                key: label,
                purpose: policy.purpose,
            })
        } else {
            None
        };
        for destination in &binding.destinations {
            if !table.binding_live(destination.binding) {
                continue;
            }
            if !table.port_live(destination.port) {
                health.local_closed = health.local_closed.saturating_add(1);
                continue;
            }
            let pending = Pending {
                binding: destination.binding,
                parent: fabric.is_some().then_some(binding.binding),
                expires_at: LinkTime(u64::MAX),
                received: Received {
                    tag: destination.tag,
                    packet: packet.clone(),
                    received_at: application.observed,
                    fabric,
                },
            };
            match destination.sender.try_send(pending) {
                Ok(()) => {}
                Err(pulsebeam_queue::TrySendError::Full(_)) => {
                    health.local_full = health.local_full.saturating_add(1);
                }
                Err(pulsebeam_queue::TrySendError::Closed(_)) => {
                    health.local_closed = health.local_closed.saturating_add(1);
                }
            }
        }
    }

    pub fn close(&mut self) {
        self.fence_receives();
        self.transport.close();
        self.pending = None;
        self.protected_relay = None;
    }

    fn ensure_live(&self) -> Result<(), Error> {
        if self.state.load(Ordering::Acquire) == network::CLOSED {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }
}
