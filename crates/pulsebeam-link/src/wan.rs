//! Explicit WAN Link adapter. It does not sniff a route into existence or fall
//! back to the historical datagram transport. The controller owns authority and
//! the shard's installation receipt; the host owns the nonfragmenting socket.

use std::{collections::BTreeMap, net::SocketAddr, num::NonZeroU32, time::Instant};

use pulsebeam_transport::{
    PacketKind, classify,
    dtls_srtp::wan::session::{Session, WindowTicket},
};

pub use pulsebeam_transport::dtls_srtp::wan::{
    Direction, Limits, MAX_DATAGRAM_BYTES,
    handoff::{Dropped, Prepared, SocketOutcome, Submission},
    path::{Binding, Lease},
    session::{
        Config, Ecn, Fingerprint, LiveDeadline, LocalIdentity, Purpose, Role, SendBudget,
        WindowSpec,
    },
};

use crate::{CodecError, Fence, LinkTime, Ordering, PeerControl, Shared, control, network};

type Send<'a> = dyn FnMut(SocketAddr, &[u8]) -> SocketOutcome + 'a;

pub trait Codec<P> {
    type Window: Copy + Eq;

    fn route(&self, window: Self::Window) -> (NonZeroU32, u64);
    fn encode(
        &self,
        window: Self::Window,
        position: u64,
        packet: &P,
        dst: &mut [u8],
    ) -> Result<usize, CodecError>;
    fn decode(&self, window: Self::Window, plaintext: &[u8]) -> Result<(u64, P), CodecError>;
}

#[derive(Debug)]
pub enum Error {
    Transport(pulsebeam_transport::dtls_srtp::wan::session::Error),
    Codec(CodecError),
    Scope,
}

impl From<pulsebeam_transport::dtls_srtp::wan::session::Error> for Error {
    fn from(error: pulsebeam_transport::dtls_srtp::wan::session::Error) -> Self {
        Self::Transport(error)
    }
}

impl From<CodecError> for Error {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

/// Exact locally staged facts. The controller must compare the authenticated
/// peer ACK and child receipt before asking Link to activate this ticket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Staged<W> {
    route: WindowTicket,
    descriptor: W,
}

impl<W: Copy> Staged<W> {
    pub fn descriptor(self) -> W {
        self.descriptor
    }

    pub fn spec(self) -> WindowSpec {
        self.route.spec()
    }
}

pub struct Received<P, W> {
    pub route: Staged<W>,
    pub expires: Instant,
    pub position: u64,
    pub packet: P,
}

/// The host provisions this separately from the legacy Link driver. A media
/// packet can be delivered only on a previously activated receive ticket.
pub struct Driver<C, W> {
    binding: Binding,
    session: Session,
    codec: C,
    origin: Instant,
    control: Option<ControlLane>,
    windows: BTreeMap<(Direction, NonZeroU32), Staged<W>>,
}

struct ControlLane {
    state: Shared<Fence>,
    host: control::HostLane,
    lease: Option<Lease>,
}

impl<C, W> Driver<C, W>
where
    W: Copy + Eq,
{
    pub fn new(config: Config<'_>, codec: C, now: Instant) -> Result<Self, Error> {
        Ok(Self {
            binding: config.binding,
            session: Session::new(config, now)?,
            codec,
            origin: now,
            control: None,
            windows: BTreeMap::new(),
        })
    }

    pub fn with_control(
        config: Config<'_>,
        codec: C,
        now: Instant,
    ) -> Result<(Self, PeerControl), Error> {
        let mut driver = Self::new(config, codec, now)?;
        let peer = driver.attach_control()?;
        Ok((driver, peer))
    }

    pub fn with_pinned_peer(
        peer: crate::NetworkPeer,
        id: crate::NetworkId,
        generation: std::num::NonZeroU64,
        limits: Limits,
        codec: C,
        now: Instant,
        origin: Instant,
    ) -> Result<(Self, PeerControl), Error> {
        let mut driver = Self::media_only(peer, id, generation, limits, codec, now)?;
        driver.origin = origin;
        let control = driver.attach_control()?;
        Ok((driver, control))
    }

    pub fn media_only(
        peer: crate::NetworkPeer,
        id: crate::NetworkId,
        generation: std::num::NonZeroU64,
        limits: Limits,
        codec: C,
        now: Instant,
    ) -> Result<Self, Error> {
        let epoch = std::num::NonZeroU64::new(id.epoch()).ok_or(Error::Scope)?;
        let peer = peer.into_transport();
        Self::new(
            peer.fabric_wan_config(epoch, generation, limits),
            codec,
            now,
        )
    }

    pub fn attach_control(&mut self) -> Result<PeerControl, Error> {
        if self.control.is_some() || self.session.is_closed() {
            return Err(Error::Scope);
        }
        let state = Shared::new(Fence::new(network::NOT_READY));
        let (peer, host) = control::pair(state.clone());
        self.control = Some(ControlLane {
            state,
            host,
            lease: None,
        });
        Ok(peer)
    }

    pub const fn binding(&self) -> Binding {
        self.binding
    }

    pub fn is_closed(&self) -> bool {
        self.session.is_closed()
    }

    pub fn lease(&self, now: Instant) -> Option<Lease> {
        self.session.lease(now)
    }

    pub fn send_budget(&self, now: Instant) -> Option<SendBudget> {
        self.session.send_budget(now)
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        let blocked_expiry = self.control.as_ref().and_then(|control| {
            let expires = control.host.pending.as_ref()?.expires_at.as_nanos();
            self.origin
                .checked_add(std::time::Duration::from_nanos(expires))
        });
        self.session
            .next_deadline()
            .into_iter()
            .chain(blocked_expiry)
            .min()
    }

    /// Only ownership submissions are retained after an explicit socket block.
    pub fn control_write_blocked(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|control| control.host.pending.is_some())
    }

    /// Wake the socket owner for an ownership frame even when UDP is idle.
    pub async fn control_readable(&mut self) {
        if let Some(control) = &mut self.control
            && control.lease.is_some()
            && control.host.pending.is_none()
            && control.host.outgoing.readable().await
        {
            return;
        }
        std::future::pending::<()>().await;
    }

    pub fn tick(&mut self, now: Instant, send: &mut Send<'_>) -> Result<(), Error> {
        let result = self.session.tick(now, send);
        self.sync_control(now);
        result?;
        if self.session.lease(now).is_none() {
            return Ok(());
        }
        let Some(control) = &mut self.control else {
            return Ok(());
        };
        let lane = &mut control.host;
        let Some(chunk) = lane
            .pending
            .take()
            .or_else(|| lane.outgoing.try_recv().ok())
        else {
            return Ok(());
        };
        let elapsed = now.saturating_duration_since(self.origin).as_nanos();
        let link_now = LinkTime::from_nanos(u64::try_from(elapsed).unwrap_or(u64::MAX));
        if chunk.expires_at <= link_now {
            return Ok(());
        }
        let result = self.session.send_control(now, &chunk.bytes, send);
        if matches!(result, Ok(SocketOutcome::WouldBlock)) {
            lane.pending = Some(chunk);
        }
        self.sync_control(now);
        match result {
            Ok(_) | Err(pulsebeam_transport::dtls_srtp::wan::session::Error::Control) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn sync_control(&mut self, now: Instant) {
        let lease = self.session.lease(now);
        if self.session.is_closed()
            || self
                .control
                .as_ref()
                .is_some_and(|control| control.lease.is_some() && control.lease != lease)
        {
            if let Some(control) = self.control.take() {
                control.state.store(network::CLOSED, Ordering::Release);
            }
            let _ = self.session.take_control();
            return;
        }
        if let Some(control) = &mut self.control {
            control.lease = lease;
            control.state.store(
                if lease.is_some() {
                    network::READY
                } else {
                    network::NOT_READY
                },
                Ordering::Release,
            );
            if let Some(frame) = self.session.take_control()
                && lease.is_some()
            {
                let _ = control.host.incoming.try_send(frame);
            }
        }
    }

    pub fn take_control(&mut self) -> Option<Box<[u8]>> {
        self.session.take_control()
    }

    pub fn stage<P>(
        &mut self,
        now: Instant,
        binding: Binding,
        spec: WindowSpec,
        descriptor: W,
    ) -> Result<Staged<W>, Error>
    where
        C: Codec<P, Window = W>,
    {
        self.stage_with_deadline(now, binding, spec, descriptor, None)
    }

    pub fn stage_with_deadline<P>(
        &mut self,
        now: Instant,
        binding: Binding,
        spec: WindowSpec,
        descriptor: W,
        deadline: Option<LiveDeadline>,
    ) -> Result<Staged<W>, Error>
    where
        C: Codec<P, Window = W>,
    {
        let key = (spec.direction, spec.ssrc);
        if self.codec.route(descriptor) != (spec.ssrc, spec.start)
            || self
                .windows
                .get(&key)
                .is_some_and(|staged| staged.descriptor != descriptor)
        {
            return Err(Error::Scope);
        }
        let route = self
            .session
            .stage_window_with_deadline(now, binding, spec, deadline)?;
        let staged = Staged { route, descriptor };
        self.windows.insert(key, staged);
        Ok(staged)
    }

    /// Compare all immutable facts, including profile/form/emission in the
    /// descriptor, before committing the staged transport ticket.
    pub fn activate(
        &mut self,
        now: Instant,
        staged: Staged<W>,
        acknowledged: W,
        spec: WindowSpec,
    ) -> Result<(), Error> {
        if staged.descriptor != acknowledged
            || staged.spec() != spec
            || self.windows.get(&(spec.direction, spec.ssrc)) != Some(&staged)
        {
            return Err(Error::Scope);
        }
        self.session
            .activate_window(now, staged.route)
            .map_err(Error::from)
    }

    pub fn retire(&mut self, staged: Staged<W>) -> bool {
        let spec = staged.spec();
        let key = (spec.direction, spec.ssrc);
        if self.windows.get(&key) != Some(&staged) {
            return false;
        }
        self.windows.remove(&key);
        self.session.retire_window(staged.route)
    }

    pub fn replace_path(&mut self, now: Instant, binding: Binding) -> bool {
        if !self.session.replace_path(now, binding) {
            return false;
        }
        self.binding = binding;
        self.windows.clear();
        if let Some(control) = self.control.take() {
            control.state.store(network::CLOSED, Ordering::Release);
        }
        true
    }

    pub fn prepare<P>(
        &mut self,
        now: Instant,
        staged: Staged<W>,
        position: u64,
        packet: &P,
    ) -> Result<Prepared, Error>
    where
        C: Codec<P, Window = W>,
    {
        self.prepare_with(now, staged, position, |codec, window, dst| {
            codec.encode(window.descriptor(), position, packet, dst)
        })
    }

    pub fn prepare_with(
        &mut self,
        now: Instant,
        staged: Staged<W>,
        position: u64,
        encode: impl FnOnce(&C, Staged<W>, &mut [u8]) -> Result<usize, CodecError>,
    ) -> Result<Prepared, Error> {
        let spec = staged.spec();
        if spec.direction != Direction::Send
            || self.windows.get(&(spec.direction, spec.ssrc)) != Some(&staged)
        {
            return Err(Error::Scope);
        }
        let mut bytes = [0; MAX_DATAGRAM_BYTES];
        let written = encode(&self.codec, staged, &mut bytes)?;
        let plaintext = bytes.get(..written).ok_or(CodecError::TooLarge)?;
        self.session
            .prepare_media(now, staged.route, position, plaintext)
            .map_err(Error::from)
    }

    pub fn ecn_marking_allowed(&self) -> bool {
        self.session.ecn_marking_allowed()
    }

    pub fn submit(&mut self, now: Instant, prepared: Prepared, send: &mut Send<'_>) -> Submission {
        self.submit_with_reserve(now, prepared, 0, send)
    }

    pub fn submit_with_reserve(
        &mut self,
        now: Instant,
        prepared: Prepared,
        reserve_bytes: usize,
        send: &mut Send<'_>,
    ) -> Submission {
        let outcome = self
            .session
            .submit_media_with_reserve(now, prepared, reserve_bytes, send);
        if matches!(outcome, Submission::Dropped(_)) {
            self.sync_control(now);
        }
        outcome
    }

    pub fn receive<P>(
        &mut self,
        now: Instant,
        binding: Binding,
        bytes: &[u8],
        send: &mut Send<'_>,
    ) -> Result<Option<Received<P, W>>, Error>
    where
        C: Codec<P, Window = W>,
    {
        self.receive_with_ecn(now, binding, bytes, None, send)
    }

    pub fn receive_with_ecn<P>(
        &mut self,
        now: Instant,
        binding: Binding,
        bytes: &[u8],
        ecn: Option<Ecn>,
        send: &mut Send<'_>,
    ) -> Result<Option<Received<P, W>>, Error>
    where
        C: Codec<P, Window = W>,
    {
        self.receive_with_decoder(now, binding, bytes, ecn, send, |codec, window, bytes| {
            codec.decode(window.descriptor(), bytes)
        })
    }

    /// Select a decoder using the exact activated ticket, after authentication.
    /// Hop metadata can be returned with the packet without becoming packet data.
    pub fn receive_with_decoder<P>(
        &mut self,
        now: Instant,
        binding: Binding,
        bytes: &[u8],
        ecn: Option<Ecn>,
        send: &mut Send<'_>,
        decode: impl FnOnce(&C, Staged<W>, &[u8]) -> Result<(u64, P), CodecError>,
    ) -> Result<Option<Received<P, W>>, Error> {
        if binding != self.binding {
            return Ok(None);
        }
        match classify(bytes, false) {
            PacketKind::Dtls => {
                let result = self.session.receive(now, binding, bytes, send);
                self.sync_control(now);
                result?;
                Ok(None)
            }
            // Fabric RTP uses only PT 96–102, so even with marker set it
            // cannot overlap the RTCP packet-type range.
            PacketKind::RtpRtcp if matches!(bytes.get(1), Some(192..=223)) => {
                let result = self.session.receive_feedback(now, binding, bytes);
                self.sync_control(now);
                result?;
                Ok(None)
            }
            PacketKind::RtpRtcp => {
                let opened = self.session.open_media_with_ecn(now, binding, bytes, ecn)?;
                let route = self
                    .windows
                    .get(&(Direction::Receive, opened.window.spec().ssrc))
                    .copied()
                    .filter(|route| route.route == opened.window)
                    .ok_or(Error::Scope)?;
                let (position, packet) = decode(&self.codec, route, &opened.plaintext)?;
                if position != opened.position {
                    return Err(Error::Scope);
                }
                Ok(Some(Received {
                    route,
                    expires: opened.expires,
                    position,
                    packet,
                }))
            }
            _ => Ok(None),
        }
    }
}

impl<C, W> Drop for Driver<C, W> {
    fn drop(&mut self) {
        if let Some(control) = &self.control {
            control.state.store(network::CLOSED, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests;
