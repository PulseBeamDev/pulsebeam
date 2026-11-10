//! Link's single-owner host adapter. Transport owns security, feedback and CC;
//! Link owns application queues, route fences, decoding and local fanout.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::network::{self, Outbound, ReceiveBinding, WireCodec};
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
    codec: Shared<dyn WireCodec<P>>,
    state: Shared<Fence>,
    table: Shared<Table>,
    outbound: pulsebeam_queue::Receiver<Outbound<P>>,
    commands: pulsebeam_queue::Receiver<ReceiveBinding<P>>,
    routes: HashMap<NodeRouteKey, ReceiveBinding<P>>,
    pending: Option<Outbound<P>>,
    now: LinkTime,
    health: Health,
}

impl<P> Drop for NetworkDriver<P> {
    fn drop(&mut self) {
        self.state.store(network::CLOSED, Ordering::Release);
    }
}

impl<P: Clone + Send + 'static> NetworkDriver<P> {
    pub(crate) fn new(
        peer: pulsebeam_transport::PeerConfig,
        codec: Shared<dyn WireCodec<P>>,
        state: Shared<Fence>,
        table: Shared<Table>,
        outbound: pulsebeam_queue::Receiver<Outbound<P>>,
        commands: pulsebeam_queue::Receiver<ReceiveBinding<P>>,
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
            now: LinkTime(0),
            health: Health::default(),
        })
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
            ready: transport.ready,
            feedback_interval: transport.feedback_interval,
            sent: transport.sent,
            received: transport.received,
            send_failed: transport.send_failed,
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

    pub async fn readable(&mut self) -> bool {
        if self.transport.needs_progress() {
            return true;
        }
        if self.pending.is_some() {
            return self.commands.readable().await;
        }
        futures_lite::future::or(self.commands.readable(), self.outbound.readable()).await
    }

    pub fn next_deadline(&self) -> Option<LinkTime> {
        [
            self.transport.next_deadline(),
            self.pending.as_ref().map(|pending| pending.expires_at),
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
        self.transport.poll(now, 0, &mut *send, |_| {})?;
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
            self.transport.set_demand(self.pending.is_some());
            let table = &self.table;
            let routes = &self.routes;
            let codec = &self.codec;
            let health = &mut self.health;
            work = work.saturating_add(self.transport.poll(
                now,
                budget.saturating_sub(work),
                &mut *send,
                |application| {
                    Self::deliver(table, routes, codec.as_ref(), health, application);
                },
            )?);
            if self.transport.health().ready {
                let _ = self.state.compare_exchange(
                    network::NOT_READY,
                    network::READY,
                    Ordering::Release,
                    Ordering::Relaxed,
                );
            }
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

    fn prepare_next(&mut self) -> Result<bool, Error> {
        let Some(pending) = &self.pending else {
            return Ok(false);
        };
        if !self.table.binding_live(pending.binding) {
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
            Err(error) => return Err(error),
        }
        let mut payload = [0u8; wire::PAYLOAD_BUDGET];
        let length = self
            .codec
            .encode(&pending.packet, &mut payload)
            .map_err(|_| Error::Codec)?;
        let payload = payload.get(..length).ok_or(Error::Codec)?;
        match self
            .transport
            .try_submit(pending.address.to_bytes(), payload, pending.expires_at)
        {
            Ok(()) => {
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
        codec: &dyn WireCodec<P>,
        health: &mut Health,
        application: pulsebeam_transport::Application<'_>,
    ) {
        let Some(binding) = routes.get(&NodeRouteKey::from_bytes(application.context)) else {
            health.stale = health.stale.saturating_add(1);
            return;
        };
        if !table.binding_live(binding.binding) {
            health.stale = health.stale.saturating_add(1);
            return;
        }
        let Ok(packet) = codec.decode(application.payload) else {
            health.invalid = health.invalid.saturating_add(1);
            return;
        };
        for destination in &binding.destinations {
            if !table.port_live(destination.port) {
                health.local_closed = health.local_closed.saturating_add(1);
                continue;
            }
            let pending = Pending {
                binding: binding.binding,
                expires_at: LinkTime(u64::MAX),
                received: Received {
                    tag: destination.tag,
                    packet: packet.clone(),
                    received_at: application.observed,
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
        self.state.store(network::CLOSED, Ordering::Release);
        self.transport.close();
        self.pending = None;
        self.routes.clear();
    }

    fn ensure_live(&self) -> Result<(), Error> {
        if self.state.load(Ordering::Acquire) == network::CLOSED {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }
}
