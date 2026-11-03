#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    reason = "SCTP wire fields use bounded arithmetic; DataChannel uses Arc strings, native config leases and Bytes messages"
)]

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::Instant,
};

use bytes::Bytes;
use dcsctp::api::{
    DcSctpSocket, LifecycleId, Message, Options, PpId, ResetStreamsError, SendError, SendOptions,
    Socket, SocketEvent, SocketState, SocketTime, StreamId,
};

use crate::{
    AcceptError, ConnectionLimits, DataChannelConfig, DataChannelEvent, DataChannelId,
    DataChannelPriority, DataChannelStats, DataMessage, DataReliability, Event, WriteError,
    negotiation::{DtlsRole, SctpSessionFacts},
};

const DCEP_PPID: PpId = PpId(50);
const TEXT_PPID: PpId = PpId(51);
const BINARY_PPID: PpId = PpId(53);
const EMPTY_TEXT_PPID: PpId = PpId(56);
const EMPTY_BINARY_PPID: PpId = PpId(57);
const DCEP_ACK: u8 = 0x02;
const DCEP_OPEN: u8 = 0x03;
const MAX_WORK_PER_CALL: usize = 256;
const LIFECYCLE_BYTES: usize = std::mem::size_of::<(u64, BufferedMessage)>();
const CHANNEL_BYTES: usize = std::mem::size_of::<(StreamId, Channel)>();
const EVENT_BYTES: usize = std::mem::size_of::<QueuedEvent>();
#[cfg(test)]
const PACKET_BYTES: usize = std::mem::size_of::<Vec<u8>>();
const CHANNEL_NOTIFICATIONS: u8 = 3;
const RESET_BYTES: usize =
    std::mem::size_of::<(StreamId, ResetState)>() + std::mem::size_of::<StreamId>();
const SCTP_MTU: usize = 1_191;
const DCEP_OPEN_HEADER_BYTES: usize = 12;
const MAX_DCEP_MESSAGE_BYTES: usize = DCEP_OPEN_HEADER_BYTES + 2 * u16::MAX as usize;

pub(crate) struct BufferLimits {
    receive_window_bytes: usize,
    send_buffer_bytes: usize,
}

pub(crate) const fn receive_message_bytes(limits: ConnectionLimits) -> usize {
    if limits.max_inbound_data_message_bytes > MAX_DCEP_MESSAGE_BYTES {
        limits.max_inbound_data_message_bytes
    } else {
        MAX_DCEP_MESSAGE_BYTES
    }
}

pub(crate) const fn buffer_limits(limits: ConnectionLimits) -> Option<BufferLimits> {
    let Some(inbound) = receive_message_bytes(limits).checked_add(SCTP_MTU) else {
        return None;
    };
    let receive_window_bytes = if inbound > limits.max_buffered_data_bytes {
        inbound
    } else {
        limits.max_buffered_data_bytes
    };
    let Some(send_buffer_bytes) = limits
        .max_buffered_data_bytes
        .checked_add(MAX_DCEP_MESSAGE_BYTES)
    else {
        return None;
    };
    // dcSCTP narrows both options into u32 window fields during establishment.
    if receive_window_bytes > u32::MAX as usize || send_buffer_bytes > u32::MAX as usize {
        return None;
    }
    Some(BufferLimits {
        receive_window_bytes,
        send_buffer_bytes,
    })
}

pub(crate) struct Association {
    socket: Socket,
    origin: Instant,
    local_stream_parity: u16,
    max_inbound_sctp_message_bytes: usize,
    max_inbound_message_bytes: usize,
    peer_max_message_bytes: usize,
    max_outbound_message_bytes: usize,
    max_buffered_bytes: usize,
    max_runtime_bytes: usize,
    max_packet_bytes: usize,
    outgoing_packet_bytes: usize,
    future_events: usize,
    channels: BTreeMap<StreamId, Channel>,
    outgoing: VecDeque<Vec<u8>>,
    events: VecDeque<QueuedEvent>,
    lifecycles: BTreeMap<u64, BufferedMessage>,
    next_lifecycle: u64,
    next_channel_lifetime: u64,
    resetting: BTreeMap<StreamId, ResetState>,
    ready_resets: BTreeSet<StreamId>,
    reset_all_cursor: Option<StreamId>,
    declared_cursor: Option<StreamId>,
    reset_runtime_bytes: usize,
    config_runtime_bytes: usize,
    buffered_payload_bytes: usize,
    queued_inbound_payload_bytes: usize,
    committed_transport_bytes: u64,
    shutdown_deadline: Option<Instant>,
    stopped: bool,
    // Native queues have no peek; an exhausted bounded drain needs a continuation.
    native_drain_exhausted: bool,
}

// SAFETY: dcsctp 0.1.14 uses Rc/RefCell only to share bookkeeping inside one
// Socket. Association uniquely owns that socket, never exposes a dcsctp handle,
// and all access requires &mut self, so moving the complete ownership graph to
// another thread cannot create concurrent access or a cross-thread drop race.
unsafe impl Send for Association {}

struct QueuedEvent {
    lifetime: u64,
    event: DataChannelEvent,
    retained_payload_bytes: usize,
    config_lease: Option<Arc<ConfigLease>>,
}

struct ConfigLease {
    bytes: usize,
}

struct Channel {
    lifetime: u64,
    id: DataChannelId,
    stream: StreamId,
    config: DataChannelConfig,
    config_lease: Arc<ConfigLease>,
    state: ChannelState,
    reserved_events: u8,
    opened_emitted: bool,
    closed_emitted: bool,
    sent_messages: u64,
    received_messages: u64,
}

#[derive(Default)]
struct ResetState {
    incoming: bool,
    outgoing: bool,
    failed: bool,
    notification_reserved: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ChannelState {
    Opening,
    Open,
    Closing,
}

struct BufferedMessage {
    lifetime: u64,
    bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AssociationStats {
    pub(crate) buffered_payload_bytes: usize,
    pub(crate) committed_transport_bytes: u64,
    pub(crate) channels: usize,
}

impl Association {
    pub(crate) fn new(
        facts: SctpSessionFacts,
        limits: ConnectionLimits,
        accepted_at: Instant,
    ) -> Result<Self, AcceptError> {
        let buffers = buffer_limits(limits).ok_or(AcceptError::InvalidConfiguration)?;
        let peer_max_message_bytes = if facts.unlimited_message_size {
            usize::MAX
        } else {
            facts.max_message_size.unwrap_or(64 * 1024)
        };
        let max_outbound_message_bytes = peer_max_message_bytes.min(limits.max_buffered_data_bytes);
        // Native defaults announce the full wire stream count without fixed
        // per-stream storage. Local records still need actual storage credit.
        let options = Options {
            local_port: facts.port,
            remote_port: facts.port,
            mtu: SCTP_MTU,
            max_message_size: peer_max_message_bytes
                .min(limits.max_buffered_data_bytes.max(MAX_DCEP_MESSAGE_BYTES)),
            max_receiver_window_buffer_size: buffers.receive_window_bytes,
            max_send_buffer_size: buffers.send_buffer_bytes,
            per_stream_send_queue_limit: buffers.send_buffer_bytes,
            total_buffered_amount_low_threshold: 0,
            enable_partial_reliability: true,
            enable_message_interleaving: true,
            ..Options::default()
        };
        Ok(Self {
            socket: Socket::new("pulsebeam-datachannels", &options),
            origin: accepted_at,
            local_stream_parity: u16::from(facts.local_dtls_role == DtlsRole::Passive),
            max_inbound_sctp_message_bytes: receive_message_bytes(limits),
            max_inbound_message_bytes: limits.max_inbound_data_message_bytes,
            peer_max_message_bytes,
            max_outbound_message_bytes,
            max_buffered_bytes: limits.max_buffered_data_bytes,
            max_runtime_bytes: limits.max_data_runtime_bytes,
            max_packet_bytes: limits.max_transport_output_bytes,
            outgoing_packet_bytes: 0,
            future_events: 0,
            channels: BTreeMap::new(),
            outgoing: VecDeque::new(),
            events: VecDeque::new(),
            lifecycles: BTreeMap::new(),
            next_lifecycle: 1,
            next_channel_lifetime: 1,
            resetting: BTreeMap::new(),
            ready_resets: BTreeSet::new(),
            reset_all_cursor: None,
            declared_cursor: None,
            reset_runtime_bytes: 0,
            config_runtime_bytes: 0,
            buffered_payload_bytes: 0,
            queued_inbound_payload_bytes: 0,
            committed_transport_bytes: 0,
            shutdown_deadline: None,
            stopped: false,
            native_drain_exhausted: false,
        })
    }

    pub(crate) fn connect(&mut self, at: Instant) {
        if self.stopped || self.socket.state() != SocketState::Closed {
            return;
        }
        self.advance_time(at);
        self.socket.connect();
        self.drain_socket();
    }

    pub(crate) fn open(
        &mut self,
        at: Instant,
        config: DataChannelConfig,
    ) -> Result<(), (WriteError, DataChannelConfig)> {
        let connected = self.socket.state() == SocketState::Connected;
        let admission = (|| {
            if self.stopped
                || (!connected
                    && (!config.negotiated
                        || config.id.is_none()
                        || self.shutdown_deadline.is_some()))
            {
                return Err(WriteError::InvalidState);
            }
            if self.reset_all_cursor.is_some() {
                return Err(WriteError::WouldBlock);
            }
            let stream = match config.id {
                Some(id) => StreamId(id.value()),
                None => self.allocate_stream().ok_or(WriteError::WouldBlock)?,
            };
            if stream.0 == u16::MAX
                || (connected
                    && self
                        .stream_capacity()
                        .is_none_or(|capacity| stream.0 >= capacity))
            {
                return Err(WriteError::InvalidState);
            }
            if self
                .resetting
                .get(&stream)
                .is_some_and(|reset| !reset.failed)
            {
                return Err(WriteError::ResetPending);
            }
            if self.channels.contains_key(&stream) || self.resetting.contains_key(&stream) {
                return Err(WriteError::InvalidState);
            }
            let open_bytes = DCEP_OPEN_HEADER_BYTES
                .saturating_add(config.label.len())
                .saturating_add(config.protocol.len());
            if config.label.len() > usize::from(u16::MAX)
                || config.protocol.len() > usize::from(u16::MAX)
                || open_bytes > MAX_DCEP_MESSAGE_BYTES
                || (!config.negotiated && open_bytes > self.peer_max_message_bytes)
                || matches!(
                    config.reliability,
                    DataReliability::MaxLifetime(lifetime)
                        if lifetime.as_millis() > u128::from(u32::MAX)
                )
            {
                return Err(WriteError::InvalidState);
            }
            let (config_bytes, events) = self
                .prepare_channel(&config)
                .ok_or(WriteError::WouldBlock)?;
            let lifetime = self.allocate_channel_lifetime()?;
            if connected {
                self.socket
                    .set_stream_priority(stream, config.priority.weight());
            }
            self.advance_time(at);
            if !config.negotiated {
                self.socket
                    .send(
                        Message::new(stream, DCEP_PPID, encode_dcep_open(&config)),
                        &SendOptions::default(),
                    )
                    .map_err(map_send_error)?;
            }
            Ok((stream, lifetime, config_bytes, events))
        })();
        let (stream, lifetime, config_bytes, events) = match admission {
            Ok(stream) => stream,
            Err(reason) => return Err((reason, config)),
        };
        let id = config
            .id
            .unwrap_or_else(|| DataChannelId::from_value(stream.0));
        let negotiated = config.negotiated;
        self.reset_runtime_bytes += RESET_BYTES;
        self.config_runtime_bytes += config_bytes;
        events.commit(&mut self.events);
        self.future_events += usize::from(CHANNEL_NOTIFICATIONS);
        self.channels.insert(
            stream,
            Channel {
                lifetime,
                id,
                stream,
                config,
                config_lease: Arc::new(ConfigLease {
                    bytes: config_bytes,
                }),
                state: if negotiated && connected {
                    ChannelState::Open
                } else {
                    ChannelState::Opening
                },
                reserved_events: CHANNEL_NOTIFICATIONS,
                opened_emitted: false,
                closed_emitted: false,
                sent_messages: 0,
                received_messages: 0,
            },
        );
        if negotiated && connected {
            self.emit_opened(stream);
        } else if negotiated {
            self.declared_cursor = Some(
                self.declared_cursor
                    .map_or(stream, |cursor| cursor.min(stream)),
            );
        } else {
            self.drain_socket();
        }
        Ok(())
    }

    pub(crate) fn send(
        &mut self,
        at: Instant,
        channel: DataChannelId,
        message: DataMessage,
    ) -> Result<(), (WriteError, DataMessage)> {
        let admission = (|| {
            let Some(entry) = self.channel(StreamId(channel.value())) else {
                return Err(WriteError::UnknownDataChannel(channel));
            };
            if self.stopped || self.reset_all_cursor.is_some() || entry.state != ChannelState::Open
            {
                return Err(WriteError::InvalidState);
            }
            let stream = entry.stream;
            let payload_bytes = match &message {
                DataMessage::Text(bytes) | DataMessage::Binary(bytes) => bytes.len(),
            };
            if payload_bytes > self.max_outbound_message_bytes
                || payload_bytes.max(1) > self.peer_max_message_bytes
            {
                return Err(WriteError::MessageTooLarge);
            }
            if self
                .buffered_payload_bytes
                .checked_add(payload_bytes)
                .is_none_or(|total| total > self.max_buffered_bytes)
            {
                return Err(WriteError::WouldBlock);
            }
            if !self.runtime_fits(LIFECYCLE_BYTES) {
                return Err(WriteError::WouldBlock);
            }
            let (ppid, payload) = match &message {
                DataMessage::Text(bytes) if bytes.is_empty() => (EMPTY_TEXT_PPID, vec![0]),
                DataMessage::Binary(bytes) if bytes.is_empty() => (EMPTY_BINARY_PPID, vec![0]),
                DataMessage::Text(bytes) => (TEXT_PPID, bytes.to_vec()),
                DataMessage::Binary(bytes) => (BINARY_PPID, bytes.to_vec()),
            };
            let lifecycle = self.next_lifecycle;
            let lifecycle_id = LifecycleId::new(lifecycle).ok_or(WriteError::WouldBlock)?;
            let options = send_options(&entry.config, lifecycle_id);
            let lifetime = entry.lifetime;
            self.advance_time(at);
            self.socket
                .send(Message::new(stream, ppid, payload), &options)
                .map_err(map_send_error)?;
            Ok((stream, lifetime, lifecycle, payload_bytes))
        })();
        let (stream, lifetime, lifecycle, payload_bytes) = match admission {
            Ok(admission) => admission,
            Err(reason) => return Err((reason, message)),
        };
        self.next_lifecycle = self.next_lifecycle.checked_add(1).unwrap_or(0);
        self.buffered_payload_bytes += payload_bytes;
        self.lifecycles.insert(
            lifecycle,
            BufferedMessage {
                lifetime,
                bytes: payload_bytes,
            },
        );
        if let Some(channel) = self.channel_mut(stream) {
            channel.sent_messages = channel.sent_messages.saturating_add(1);
        }
        self.drain_socket();
        Ok(())
    }

    pub(crate) fn close_channel(
        &mut self,
        at: Instant,
        id: DataChannelId,
    ) -> Result<(), WriteError> {
        let stream = StreamId(id.value());
        if self.channel(stream).is_none() {
            return Err(WriteError::UnknownDataChannel(id));
        }
        self.advance_time(at);
        let result = self.begin_reset(stream);
        self.drain_socket();
        result
    }

    pub(crate) fn handle_input(&mut self, at: Instant, packet: &[u8]) {
        if self.stopped {
            return;
        }
        self.advance_time(at);
        self.socket.handle_input(packet);
        self.drain_socket();
    }

    pub(crate) fn poll_event(&mut self, at: Instant) -> Option<Event> {
        if !self.stopped {
            self.advance_time(at);
            self.drain_socket();
            self.enforce_shutdown_deadline(at);
        }
        for _ in 0..MAX_WORK_PER_CALL {
            let mut queued = self.events.pop_front()?;
            self.release_idle_events();
            if let Some(lease) = queued.config_lease.take() {
                self.release_config(lease);
            }
            self.queued_inbound_payload_bytes = self
                .queued_inbound_payload_bytes
                .saturating_sub(queued.retained_payload_bytes);
            let (id, delivery) = match &queued.event {
                DataChannelEvent::Opened { channel, .. }
                | DataChannelEvent::Message { channel, .. } => (*channel, true),
                DataChannelEvent::Closed { channel }
                | DataChannelEvent::ResetComplete { channel }
                | DataChannelEvent::ResetFailed { channel } => (*channel, false),
            };
            let channel = self.channel(StreamId(id.value()));
            let eligible = if delivery {
                !self.stopped
                    && self.reset_all_cursor.is_none()
                    && channel.is_some_and(|c| {
                        c.lifetime == queued.lifetime && c.state == ChannelState::Open
                    })
            } else {
                channel.is_none_or(|c| c.lifetime == queued.lifetime)
            };
            if eligible {
                return Some(Event::DataChannel(queued.event));
            }
        }
        None
    }

    pub(crate) fn has_pending_events(&self) -> bool {
        !self.events.is_empty() || (!self.stopped && self.native_drain_exhausted)
    }

    pub(crate) fn has_packet(&self) -> bool {
        !self.stopped && !self.outgoing.is_empty()
    }

    pub(crate) fn poll_packet(&mut self) -> Option<Vec<u8>> {
        let packet = self.outgoing.pop_front()?;
        self.outgoing_packet_bytes -= packet.capacity();
        self.release_idle_packets();
        Some(packet)
    }

    fn queue_packet(&mut self, packet: Vec<u8>) -> Result<(), Vec<u8>> {
        let admission = (|| {
            if self.stopped {
                return None;
            }
            let bytes = self.outgoing_packet_bytes.checked_add(packet.capacity())?;
            let backing = crate::event_buffer::Reservation::prepare(
                &self.outgoing,
                self.outgoing.len().checked_add(1)?,
                self.max_packet_bytes.checked_sub(bytes)?,
            )?;
            Some((bytes, backing))
        })();
        let Some((bytes, backing)) = admission else {
            return Err(packet);
        };
        backing.commit(&mut self.outgoing);
        self.outgoing_packet_bytes = bytes;
        self.outgoing.push_back(packet);
        Ok(())
    }

    fn clear_packets(&mut self) {
        self.outgoing = VecDeque::new();
        self.outgoing_packet_bytes = 0;
    }

    fn release_idle_packets(&mut self) {
        if self.outgoing.is_empty() && self.channels.is_empty() {
            self.outgoing = VecDeque::new();
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if self.stopped {
            return None;
        }
        let socket_deadline = self.socket.poll_timeout();
        let socket_deadline = (socket_deadline != SocketTime::infinite_future())
            .then(|| {
                self.origin
                    .checked_add(socket_deadline - SocketTime::zero())
            })
            .flatten();
        match (socket_deadline, self.shutdown_deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, None) | (None, left) => left,
        }
    }

    pub(crate) fn begin_shutdown(&mut self, at: Instant, deadline: Instant) {
        if self.stopped {
            return;
        }
        self.advance_time(at);
        self.shutdown_deadline = Some(deadline);
        self.socket.shutdown();
        self.drain_socket();
        self.enforce_shutdown_deadline(at);
    }

    pub(crate) fn abort(&mut self) {
        if !self.stopped {
            self.socket.close();
        }
        self.clear_packets();
        while let Some(event) = self.events.pop_front() {
            if let Some(lease) = event.config_lease {
                self.release_config(lease);
            }
        }
        self.clear_channels();
        self.lifecycles.clear();
        self.future_events -= self
            .resetting
            .values()
            .filter(|reset| reset.notification_reserved)
            .count();
        self.resetting.clear();
        self.release_idle_events();
        self.ready_resets.clear();
        self.reset_all_cursor = None;
        self.reset_runtime_bytes = 0;
        self.buffered_payload_bytes = 0;
        self.queued_inbound_payload_bytes = 0;
        self.stopped = true;
    }

    pub(crate) const fn is_stopped(&self) -> bool {
        self.stopped
    }

    pub(crate) fn commit_transport_bytes(&mut self, bytes: usize) {
        self.committed_transport_bytes = self
            .committed_transport_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    pub(crate) fn stats(&self) -> AssociationStats {
        AssociationStats {
            buffered_payload_bytes: self.buffered_payload_bytes,
            committed_transport_bytes: self.committed_transport_bytes,
            channels: self.channels.len(),
        }
    }

    pub(crate) fn channel_stats(&self) -> Vec<DataChannelStats> {
        self.channels
            .values()
            .map(|channel| DataChannelStats {
                channel: channel.id,
                priority: channel.config.priority,
                reliability: channel.config.reliability,
                buffered_amount: self
                    .lifecycles
                    .values()
                    .filter(|message| message.lifetime == channel.lifetime)
                    .map(|message| message.bytes)
                    .fold(0_usize, usize::saturating_add),
                queued_payload_bytes: self.socket.buffered_amount(channel.stream),
                sent_messages: channel.sent_messages,
                received_messages: channel.received_messages,
            })
            .collect()
    }

    pub(crate) fn stream_capacity(&self) -> Option<u16> {
        if self.stopped || self.socket.state() != SocketState::Connected {
            return None;
        }
        self.socket.get_metrics().map(|metrics| {
            metrics
                .negotiated_maximum_incoming_streams
                .min(metrics.negotiated_maximum_outgoing_streams)
        })
    }

    fn activate_declared(&mut self, stream: StreamId) {
        if self.reset_all_cursor.is_some() || self.shutdown_deadline.is_some() || self.stopped {
            return;
        }
        let Some(channel) = self.channel(stream) else {
            return;
        };
        if !channel.config.negotiated || channel.state != ChannelState::Opening {
            return;
        }
        let priority = channel.config.priority.weight();
        let Some(capacity) = self.stream_capacity() else {
            return;
        };
        if stream.0 >= capacity {
            self.reject_declared(stream);
            return;
        }
        self.socket.set_stream_priority(stream, priority);
        if let Some(channel) = self.channel_mut(stream) {
            channel.state = ChannelState::Open;
        }
        self.emit_opened(stream);
    }

    fn reject_declared(&mut self, stream: StreamId) {
        // No reset barrier completed: retain only the funded non-reuse guard.
        self.emit_closed(stream);
        self.resetting.insert(stream, ResetState::default());
        self.fail_reset(stream);
        if let Some(channel) = self.channels.remove(&stream) {
            self.future_events -= usize::from(channel.reserved_events);
            self.release_config(channel.config_lease);
        }
        self.release_idle_events();
    }

    fn visit_declared(&mut self) {
        let Some(cursor) = self.declared_cursor else {
            return;
        };
        if self.reset_all_cursor.is_some() || self.socket.state() != SocketState::Connected {
            return;
        }
        if let Some(stream) = self
            .channels
            .range(cursor..)
            .next()
            .map(|(&stream, _)| stream)
        {
            if self.shutdown_deadline.is_some() {
                if self.channel(stream).is_some_and(|channel| {
                    channel.config.negotiated && channel.state == ChannelState::Opening
                }) {
                    self.reject_declared(stream);
                }
            } else {
                self.activate_declared(stream);
            }
            self.declared_cursor = stream.0.checked_add(1).map(StreamId);
        } else {
            self.declared_cursor = None;
        }
    }

    fn allocate_stream(&self) -> Option<StreamId> {
        (self.local_stream_parity..self.stream_capacity()?)
            .step_by(2)
            .map(StreamId)
            .find(|stream| {
                !self.channels.contains_key(stream) && !self.resetting.contains_key(stream)
            })
    }

    fn advance_time(&mut self, now: Instant) {
        self.socket
            .advance_time(SocketTime::zero() + now.saturating_duration_since(self.origin));
    }

    fn drain_socket(&mut self) {
        self.native_drain_exhausted = false;
        for _ in 0..MAX_WORK_PER_CALL {
            self.visit_reset_all();
            self.visit_declared();
            let event = self.socket.poll_event();
            let progressed = event.is_some();
            if let Some(event) = event {
                match event {
                    SocketEvent::SendPacket(packet) => {
                        if self.queue_packet(packet).is_err() {
                            self.abort_association();
                            break;
                        }
                    }
                    SocketEvent::OnClosed() | SocketEvent::OnAborted(_, _) => {
                        self.close_all_channels();
                        self.stopped = true;
                        break;
                    }
                    SocketEvent::OnConnected()
                    | SocketEvent::OnConnectionRestarted()
                    | SocketEvent::OnError(_, _)
                    | SocketEvent::OnBufferedAmountLow(_)
                    | SocketEvent::OnTotalBufferedAmountLow()
                    | SocketEvent::OnLifecycleMessageFullySent(_)
                    | SocketEvent::OnLifecycleMessageMaybeExpired(_)
                    | SocketEvent::OnLifecycleMessageExpired(_)
                    | SocketEvent::OnLifecycleMessageDelivered(_) => {}
                    SocketEvent::OnStreamsResetFailed(streams) => {
                        for stream in streams {
                            self.fail_reset(stream);
                        }
                    }
                    SocketEvent::OnStreamsResetPerformed(streams) => {
                        for stream in streams {
                            if let Some(reset) = self.resetting.get_mut(&stream) {
                                reset.outgoing = true;
                                self.queue_reset_completion(stream);
                            }
                        }
                    }
                    SocketEvent::OnIncomingStreamReset(streams) => {
                        if streams.is_empty() {
                            self.reset_all_cursor = Some(StreamId(0));
                        } else {
                            for stream in streams {
                                self.incoming_reset(stream);
                            }
                        }
                    }
                    SocketEvent::OnLifecycleEnd(id) => {
                        if let Some(message) = self.lifecycles.remove(&id.value()) {
                            self.buffered_payload_bytes =
                                self.buffered_payload_bytes.saturating_sub(message.bytes);
                        }
                    }
                }
            }
            // CookieEchoed can reassemble DATA before COOKIE ACK supplies stream facts.
            // Keep that DATA in the native receive FIFO until declarations can activate.
            let connecting = self.socket.state() == SocketState::Connecting;
            let message = if connecting {
                None
            } else {
                self.socket.get_next_message()
            };
            if let Some(message) = message {
                self.handle_message(message);
            } else if !progressed {
                if self.reset_all_cursor.is_some()
                    || (self.declared_cursor.is_some()
                        && self.socket.state() == SocketState::Connected)
                {
                    continue;
                }
                if !connecting {
                    self.complete_resets();
                }
                return;
            }
        }
        self.native_drain_exhausted = !self.stopped;
    }

    fn handle_message(&mut self, message: Message) {
        if self.stopped {
            return;
        }
        let stream = message.stream_id;
        if message.payload.len() > self.max_inbound_sctp_message_bytes {
            self.close_stream(stream);
            return;
        }
        match message.ppid {
            DCEP_PPID => {
                if message.payload.len() > MAX_DCEP_MESSAGE_BYTES {
                    self.close_stream(stream);
                } else {
                    self.handle_dcep(stream, &message.payload);
                }
            }
            TEXT_PPID | BINARY_PPID | EMPTY_TEXT_PPID | EMPTY_BINARY_PPID => {
                if self.declared_cursor.is_some() {
                    self.activate_declared(stream);
                }
                let Some(channel) = self.channel(stream) else {
                    self.close_stream(stream);
                    return;
                };
                if self.reset_all_cursor.is_some() || channel.state != ChannelState::Open {
                    return;
                }
                let empty = message.ppid == EMPTY_TEXT_PPID || message.ppid == EMPTY_BINARY_PPID;
                let semantic_len = if empty { 0 } else { message.payload.len() };
                if semantic_len > self.max_inbound_message_bytes {
                    self.close_stream(stream);
                    return;
                }
                let retained_payload_bytes = if empty { 0 } else { message.payload.capacity() };
                let id = channel.id;
                let bytes = if empty {
                    Bytes::new()
                } else {
                    Bytes::from(message.payload)
                };
                let message = if message.ppid == BINARY_PPID || message.ppid == EMPTY_BINARY_PPID {
                    DataMessage::Binary(bytes)
                } else {
                    DataMessage::Text(bytes)
                };
                if self.push_event(
                    DataChannelEvent::Message {
                        channel: id,
                        message,
                    },
                    retained_payload_bytes,
                ) && let Some(channel) = self.channel_mut(stream)
                {
                    channel.received_messages = channel.received_messages.saturating_add(1);
                }
            }
            _ => self.close_stream(stream),
        }
    }

    fn handle_dcep(&mut self, stream: StreamId, payload: &[u8]) {
        if self.reset_all_cursor.is_some() {
            return;
        }
        match payload.first().copied() {
            Some(DCEP_ACK) if payload == [DCEP_ACK] => {
                if let Some(channel) = self.channel_mut(stream)
                    && channel.state == ChannelState::Opening
                    && !channel.config.negotiated
                {
                    channel.state = ChannelState::Open;
                    self.emit_opened(stream);
                }
            }
            Some(DCEP_OPEN) => {
                if let Some(channel) = self.channel(stream) {
                    if channel.state == ChannelState::Open {
                        let _ = self.socket.send(
                            Message::new(stream, DCEP_PPID, vec![DCEP_ACK]),
                            &SendOptions::default(),
                        );
                    }
                    return;
                }
                if stream.0 % 2 == self.local_stream_parity
                    || self.resetting.contains_key(&stream)
                    || !self.runtime_fits(
                        CHANNEL_BYTES + RESET_BYTES + std::mem::size_of::<ConfigLease>(),
                    )
                {
                    self.close_stream(stream);
                    return;
                }
                let Some(config) = decode_dcep_open(payload) else {
                    self.close_stream(stream);
                    return;
                };
                let Some((config_bytes, events)) = self.prepare_channel(&config) else {
                    self.close_stream(stream);
                    return;
                };
                let id = DataChannelId::from_value(stream.0);
                let Ok(lifetime) = self.allocate_channel_lifetime() else {
                    self.close_stream(stream);
                    return;
                };
                self.socket
                    .set_stream_priority(stream, config.priority.weight());
                self.reset_runtime_bytes += RESET_BYTES;
                self.config_runtime_bytes += config_bytes;
                events.commit(&mut self.events);
                self.future_events += usize::from(CHANNEL_NOTIFICATIONS);
                self.channels.insert(
                    stream,
                    Channel {
                        lifetime,
                        id,
                        stream,
                        config,
                        config_lease: Arc::new(ConfigLease {
                            bytes: config_bytes,
                        }),
                        state: ChannelState::Open,
                        reserved_events: CHANNEL_NOTIFICATIONS,
                        opened_emitted: false,
                        closed_emitted: false,
                        sent_messages: 0,
                        received_messages: 0,
                    },
                );
                let _ = self.socket.send(
                    Message::new(stream, DCEP_PPID, vec![DCEP_ACK]),
                    &SendOptions::default(),
                );
                self.emit_opened(stream);
            }
            _ => self.close_stream(stream),
        }
    }

    fn close_stream(&mut self, stream: StreamId) {
        let _ = self.begin_reset(stream);
    }

    fn begin_reset(&mut self, stream: StreamId) -> Result<(), WriteError> {
        if self.stopped {
            return Err(WriteError::InvalidState);
        }
        if let Some(reset) = self.resetting.get(&stream) {
            return if reset.failed {
                Err(WriteError::InvalidState)
            } else {
                Ok(())
            };
        }
        if self.channel(stream).is_some_and(|channel| {
            channel.config.negotiated && channel.state == ChannelState::Opening
        }) {
            self.reject_declared(stream);
            return Err(WriteError::InvalidState);
        }
        let notification_reserved = if let Some(channel) = self.channel_mut(stream) {
            channel.state = ChannelState::Closing;
            false
        } else {
            let Some(events) = self.prepare_events(RESET_BYTES, 1) else {
                self.abort_association();
                return Err(WriteError::WouldBlock);
            };
            events.commit(&mut self.events);
            self.future_events += 1;
            self.reset_runtime_bytes += RESET_BYTES;
            true
        };
        self.emit_closed(stream);
        if self.stopped {
            return Err(WriteError::InvalidState);
        }
        self.resetting.insert(
            stream,
            ResetState {
                incoming: self.reset_all_cursor.is_some(),
                notification_reserved,
                ..ResetState::default()
            },
        );
        match self.socket.reset_streams(&[stream]) {
            Ok(()) => Ok(()),
            Err(ResetStreamsError::NotConnected | ResetStreamsError::NotSupported) => {
                self.fail_reset(stream);
                Err(WriteError::InvalidState)
            }
        }
    }

    fn incoming_reset(&mut self, stream: StreamId) {
        let _ = self.begin_reset(stream);
        if let Some(reset) = self.resetting.get_mut(&stream) {
            reset.incoming = true;
            self.queue_reset_completion(stream);
        }
    }

    fn visit_reset_all(&mut self) {
        let Some(cursor) = self.reset_all_cursor else {
            return;
        };
        let next = self
            .channels
            .range(cursor..)
            .next()
            .map(|(&stream, _)| stream)
            .into_iter()
            .chain(
                self.resetting
                    .range(cursor..)
                    .next()
                    .map(|(&stream, _)| stream),
            )
            .min();
        if let Some(stream) = next {
            self.incoming_reset(stream);
            self.reset_all_cursor = stream.0.checked_add(1).map(StreamId);
        } else {
            self.reset_all_cursor = None;
        }
    }

    fn fail_reset(&mut self, stream: StreamId) {
        if let Some(reset) = self.resetting.get_mut(&stream) {
            if reset.failed {
                return;
            }
            reset.failed = true;
            self.ready_resets.remove(&stream);
            self.push_reset_outcome(stream, false);
        }
    }

    fn push_reset_outcome(&mut self, stream: StreamId, success: bool) {
        let channel = DataChannelId::from_value(stream.0);
        let event = if success {
            DataChannelEvent::ResetComplete { channel }
        } else {
            DataChannelEvent::ResetFailed { channel }
        };
        if self.channels.contains_key(&stream) {
            self.push_event(event, 0);
        } else if let Some(reset) = self.resetting.get_mut(&stream)
            && reset.notification_reserved
        {
            reset.notification_reserved = false;
            self.future_events -= 1;
            // Zero is never an admitted lifetime. A later channel suppresses this
            // unknown-stream notice; otherwise it wakes a reset-blocked opener.
            self.events.push_back(QueuedEvent {
                lifetime: 0,
                event,
                retained_payload_bytes: 0,
                config_lease: None,
            });
        }
    }

    fn queue_reset_completion(&mut self, stream: StreamId) {
        if self
            .resetting
            .get(&stream)
            .is_some_and(|reset| reset.incoming && reset.outgoing && !reset.failed)
        {
            self.ready_resets.insert(stream);
        }
    }

    fn complete_resets(&mut self) {
        // dcSCTP reset callbacks do not purge its separate reassembled-message
        // FIFO. Keep the old lifetime closed until that FIFO has been drained.
        for _ in 0..MAX_WORK_PER_CALL {
            let Some(stream) = self.ready_resets.pop_first() else {
                return;
            };
            self.push_reset_outcome(stream, true);
            if let Some(channel) = self.channels.remove(&stream) {
                self.future_events -= usize::from(channel.reserved_events);
                self.release_config(channel.config_lease);
            }
            self.resetting.remove(&stream);
            self.reset_runtime_bytes -= RESET_BYTES;
            self.release_idle_events();
            self.release_idle_packets();
        }
        self.native_drain_exhausted = !self.ready_resets.is_empty();
    }

    fn emit_opened(&mut self, stream: StreamId) {
        let Some(channel) = self.channel_mut(stream) else {
            return;
        };
        if channel.opened_emitted {
            return;
        }
        channel.opened_emitted = true;
        let id = channel.id;
        let mut config = channel.config.clone();
        config.id = Some(id);
        self.push_event(
            DataChannelEvent::Opened {
                channel: id,
                config,
            },
            0,
        );
    }

    fn emit_closed(&mut self, stream: StreamId) {
        let Some(channel) = self.channel_mut(stream) else {
            return;
        };
        if channel.closed_emitted {
            return;
        }
        channel.closed_emitted = true;
        let canceled = usize::from(!channel.opened_emitted);
        channel.reserved_events -= u8::from(!channel.opened_emitted);
        let id = channel.id;
        self.future_events -= canceled;
        self.push_event(DataChannelEvent::Closed { channel: id }, 0);
    }

    fn close_all_channels(&mut self) {
        let streams: Vec<_> = self.channels.keys().copied().collect();
        for stream in streams {
            self.emit_closed(stream);
        }
    }

    fn abort_association(&mut self) {
        self.socket.close();
        self.clear_packets();
        self.close_all_channels();
        self.stopped = true;
    }

    fn enforce_shutdown_deadline(&mut self, now: Instant) {
        if self
            .shutdown_deadline
            .is_some_and(|deadline| now >= deadline)
            && !self.stopped
        {
            self.abort_association();
        }
    }

    fn channel(&self, stream: StreamId) -> Option<&Channel> {
        self.channels.get(&stream)
    }

    fn channel_mut(&mut self, stream: StreamId) -> Option<&mut Channel> {
        self.channels.get_mut(&stream)
    }

    fn clear_channels(&mut self) {
        self.declared_cursor = None;
        for (_, channel) in std::mem::take(&mut self.channels) {
            self.future_events -= usize::from(channel.reserved_events);
            self.release_config(channel.config_lease);
        }
        self.release_idle_events();
        self.release_idle_packets();
    }

    fn release_idle_events(&mut self) {
        if self.events.is_empty() && self.future_events == 0 {
            self.events = VecDeque::new();
        }
    }

    fn release_config(&mut self, lease: Arc<ConfigLease>) {
        // Lease handles never leave this association; only native holders count.
        if Arc::strong_count(&lease) == 1 {
            self.config_runtime_bytes -= lease.bytes;
        }
    }

    fn prepare_channel(
        &self,
        config: &DataChannelConfig,
    ) -> Option<(usize, crate::event_buffer::Reservation<QueuedEvent>)> {
        let bytes = std::mem::size_of::<ConfigLease>()
            .checked_add(config.label.len())?
            .checked_add(config.protocol.len())?;
        let events = self.prepare_events(
            bytes.checked_add(CHANNEL_BYTES + RESET_BYTES)?,
            usize::from(CHANNEL_NOTIFICATIONS),
        )?;
        Some((bytes, events))
    }

    fn prepare_events(
        &self,
        additional_bytes: usize,
        additional_slots: usize,
    ) -> Option<crate::event_buffer::Reservation<QueuedEvent>> {
        let base = self
            .runtime_bytes()?
            .checked_sub(self.events.capacity().checked_mul(EVENT_BYTES)?)?
            .checked_add(additional_bytes)?;
        let required = self
            .events
            .len()
            .checked_add(self.future_events)?
            .checked_add(additional_slots)?;
        crate::event_buffer::Reservation::prepare(
            &self.events,
            required,
            self.max_runtime_bytes.checked_sub(base)?,
        )
    }

    fn runtime_bytes(&self) -> Option<usize> {
        self.lifecycles
            .len()
            .checked_mul(LIFECYCLE_BYTES)?
            .checked_add(self.channels.len().checked_mul(CHANNEL_BYTES)?)?
            .checked_add(self.reset_runtime_bytes)?
            .checked_add(self.config_runtime_bytes)?
            .checked_add(self.events.capacity().checked_mul(EVENT_BYTES)?)
    }

    fn runtime_fits(&self, additional: usize) -> bool {
        self.runtime_bytes()
            .and_then(|bytes| bytes.checked_add(additional))
            .is_some_and(|bytes| bytes <= self.max_runtime_bytes)
    }

    #[cfg(test)]
    fn test_packet_bytes(&self) -> usize {
        let packets = self.outgoing.iter().map(Vec::capacity).sum::<usize>();
        assert_eq!(packets, self.outgoing_packet_bytes);
        let bytes = packets + self.outgoing.capacity() * PACKET_BYTES;
        assert!(bytes <= self.max_packet_bytes);
        bytes
    }

    #[cfg(test)]
    fn test_runtime_bytes(&self) -> usize {
        self.test_packet_bytes();
        let configs: BTreeMap<_, _> = self
            .channels
            .values()
            .map(|channel| &channel.config_lease)
            .chain(
                self.events
                    .iter()
                    .filter_map(|event| event.config_lease.as_ref()),
            )
            .map(|lease| (Arc::as_ptr(lease), lease.bytes))
            .collect();
        assert_eq!(self.config_runtime_bytes, configs.values().sum::<usize>());
        let bytes = self.lifecycles.len() * LIFECYCLE_BYTES
            + self.channels.len() * CHANNEL_BYTES
            + self.reset_runtime_bytes
            + self.config_runtime_bytes
            + self.events.capacity() * EVENT_BYTES;
        assert_eq!(
            self.future_events,
            self.channels
                .values()
                .map(|channel| usize::from(channel.reserved_events))
                .sum::<usize>()
                + self
                    .resetting
                    .values()
                    .filter(|reset| reset.notification_reserved)
                    .count()
        );
        assert!(self.events.capacity() >= self.events.len() + self.future_events);
        assert_eq!(self.runtime_bytes(), Some(bytes));
        assert!(self.runtime_fits(0));
        bytes
    }

    #[cfg(test)]
    fn test_reset_runtime_bytes(&self) -> usize {
        let identities: BTreeSet<_> = self.channels.keys().chain(self.resetting.keys()).collect();
        assert_eq!(self.reset_runtime_bytes, identities.len() * RESET_BYTES);
        assert!(
            self.ready_resets
                .iter()
                .all(|stream| self.resetting.contains_key(stream))
        );
        self.test_runtime_bytes();
        self.reset_runtime_bytes
    }

    fn allocate_channel_lifetime(&mut self) -> Result<u64, WriteError> {
        let lifetime = self.next_channel_lifetime;
        if lifetime == 0 {
            return Err(WriteError::WouldBlock);
        }
        self.next_channel_lifetime = lifetime.checked_add(1).unwrap_or(0);
        Ok(lifetime)
    }

    fn push_event(&mut self, event: DataChannelEvent, retained_payload_bytes: usize) -> bool {
        let (DataChannelEvent::Opened { channel, .. }
        | DataChannelEvent::Message { channel, .. }
        | DataChannelEvent::Closed { channel }
        | DataChannelEvent::ResetComplete { channel }
        | DataChannelEvent::ResetFailed { channel }) = &event;
        let Some(entry) = self.channel(StreamId(channel.value())) else {
            return false;
        };
        let lifetime = entry.lifetime;
        let Some(payload_bytes) = self
            .queued_inbound_payload_bytes
            .checked_add(retained_payload_bytes)
            .filter(|bytes| *bytes <= self.max_buffered_bytes)
        else {
            self.close_stream(StreamId(channel.value()));
            return false;
        };
        let notification = !matches!(event, DataChannelEvent::Message { .. });
        let backing = if notification && entry.reserved_events == 0 {
            None
        } else {
            self.prepare_events(0, usize::from(!notification))
        };
        let Some(backing) = backing else {
            if notification {
                self.socket.close();
                self.clear_packets();
                self.clear_channels();
                self.lifecycles.clear();
                self.reset_runtime_bytes = self.resetting.len() * RESET_BYTES;
                self.buffered_payload_bytes = 0;
                self.stopped = true;
            } else {
                self.close_stream(StreamId(channel.value()));
            }
            return false;
        };
        backing.commit(&mut self.events);
        let Some(entry) = self.channels.get_mut(&StreamId(channel.value())) else {
            return false;
        };
        if notification {
            entry.reserved_events -= 1;
            self.future_events -= 1;
        }
        let config_lease = matches!(event, DataChannelEvent::Opened { .. })
            .then(|| Arc::clone(&entry.config_lease));
        self.queued_inbound_payload_bytes = payload_bytes;
        self.events.push_back(QueuedEvent {
            lifetime,
            event,
            retained_payload_bytes,
            config_lease,
        });
        true
    }
}

fn send_options(config: &DataChannelConfig, lifecycle_id: LifecycleId) -> SendOptions {
    let mut options = SendOptions {
        unordered: !config.ordered,
        lifecycle_id: Some(lifecycle_id),
        ..SendOptions::default()
    };
    match config.reliability {
        DataReliability::Reliable => {}
        DataReliability::MaxRetransmits(count) => options.max_retransmissions = Some(count),
        DataReliability::MaxLifetime(lifetime) => options.lifetime = Some(lifetime),
    }
    options
}

fn map_send_error(error: SendError) -> WriteError {
    match error {
        SendError::MessageTooLarge { .. } => WriteError::MessageTooLarge,
        SendError::ResourceExhaustion => WriteError::WouldBlock,
        SendError::EmptyPayload | SendError::ShuttingDown => WriteError::InvalidState,
    }
}

fn encode_dcep_open(config: &DataChannelConfig) -> Vec<u8> {
    let (kind, parameter) = match config.reliability {
        DataReliability::Reliable => (0_u8, 0_u32),
        DataReliability::MaxRetransmits(count) => (1, u32::from(count)),
        DataReliability::MaxLifetime(lifetime) => {
            (2, u32::try_from(lifetime.as_millis()).unwrap_or(u32::MAX))
        }
    };
    let label = config.label.as_bytes();
    let protocol = config.protocol.as_bytes();
    let mut output = Vec::with_capacity(DCEP_OPEN_HEADER_BYTES + label.len() + protocol.len());
    output.push(DCEP_OPEN);
    output.push(kind | if config.ordered { 0 } else { 0x80 });
    output.extend_from_slice(&config.priority.weight().to_be_bytes());
    output.extend_from_slice(&parameter.to_be_bytes());
    output.extend_from_slice(&u16::try_from(label.len()).unwrap_or(u16::MAX).to_be_bytes());
    output.extend_from_slice(
        &u16::try_from(protocol.len())
            .unwrap_or(u16::MAX)
            .to_be_bytes(),
    );
    output.extend_from_slice(label);
    output.extend_from_slice(protocol);
    output
}

fn decode_dcep_open(payload: &[u8]) -> Option<DataChannelConfig> {
    let header: &[u8; DCEP_OPEN_HEADER_BYTES] =
        payload.get(..DCEP_OPEN_HEADER_BYTES)?.try_into().ok()?;
    if header[0] != DCEP_OPEN || header[1] & 0x7c != 0 {
        return None;
    }
    let label_len = usize::from(u16::from_be_bytes([header[8], header[9]]));
    let protocol_len = usize::from(u16::from_be_bytes([header[10], header[11]]));
    let expected = DCEP_OPEN_HEADER_BYTES
        .checked_add(label_len)?
        .checked_add(protocol_len)?;
    if payload.len() != expected {
        return None;
    }
    let priority =
        DataChannelPriority::new(u16::from_be_bytes([header[2], header[3]]).max(1)).ok()?;
    let parameter = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let reliability = match header[1] & 0x03 {
        0 if parameter == 0 => DataReliability::Reliable,
        1 if parameter <= u32::from(u16::MAX) => {
            DataReliability::MaxRetransmits(u16::try_from(parameter).ok()?)
        }
        2 => DataReliability::MaxLifetime(std::time::Duration::from_millis(u64::from(parameter))),
        _ => return None,
    };
    let label_end = DCEP_OPEN_HEADER_BYTES + label_len;
    let label = std::str::from_utf8(payload.get(DCEP_OPEN_HEADER_BYTES..label_end)?).ok()?;
    let protocol = std::str::from_utf8(payload.get(label_end..expected)?).ok()?;
    Some(DataChannelConfig {
        id: None,
        label: Arc::from(label),
        protocol: Arc::from(protocol),
        ordered: header[1] & 0x80 == 0,
        reliability,
        priority,
        negotiated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn config(reliability: DataReliability, ordered: bool) -> DataChannelConfig {
        DataChannelConfig {
            id: None,
            label: Arc::from("events"),
            protocol: Arc::from("pulsebeam"),
            ordered,
            reliability,
            priority: DataChannelPriority::HIGH,
            negotiated: false,
        }
    }

    #[test]
    fn data_channel_dcep_round_trips_every_reliability_mode() {
        for (reliability, ordered) in [
            (DataReliability::Reliable, true),
            (DataReliability::MaxRetransmits(3), false),
            (
                DataReliability::MaxLifetime(Duration::from_millis(90)),
                true,
            ),
        ] {
            let input = config(reliability, ordered);
            let output = decode_dcep_open(&encode_dcep_open(&input)).expect("valid DCEP");
            assert_eq!(output.label, input.label);
            assert_eq!(output.protocol, input.protocol);
            assert_eq!(output.ordered, input.ordered);
            assert_eq!(output.reliability, input.reliability);
            assert_eq!(output.priority, input.priority);
        }
        let mut input = config(DataReliability::Reliable, true);
        input.label = Arc::from(format!("{}a", "é".repeat(32_767)));
        input.protocol = Arc::from(format!("{}abc", "🦀".repeat(16_383)));
        assert_eq!(input.label.len(), usize::from(u16::MAX));
        assert_eq!(input.protocol.len(), usize::from(u16::MAX));
        let payload = encode_dcep_open(&input);
        assert_eq!(MAX_DCEP_MESSAGE_BYTES, 131_082);
        assert_eq!(payload.len(), MAX_DCEP_MESSAGE_BYTES);
        assert_eq!(payload.get(8..12), Some([255_u8; 4].as_slice()));
        assert_eq!(decode_dcep_open(&payload), Some(input));
    }

    #[test]
    fn data_channel_malformed_dcep_is_rejected() {
        let mut payload = encode_dcep_open(&config(DataReliability::Reliable, true));
        payload[1] = 0x7f;
        assert!(decode_dcep_open(&payload).is_none());
        payload = encode_dcep_open(&config(DataReliability::Reliable, true));
        payload.pop();
        assert!(decode_dcep_open(&payload).is_none());
        let original = config(DataReliability::Reliable, true);
        let mut payload = encode_dcep_open(&original);
        payload[DCEP_OPEN_HEADER_BYTES] = 0xff;
        assert!(decode_dcep_open(&payload).is_none());
        let mut payload = encode_dcep_open(&original);
        payload[DCEP_OPEN_HEADER_BYTES + original.label.len()] = 0xff;
        assert!(decode_dcep_open(&payload).is_none());
        let mut payload = encode_dcep_open(&original);
        payload.push(0);
        assert!(decode_dcep_open(&payload).is_none());
    }

    fn facts(role: DtlsRole) -> SctpSessionFacts {
        SctpSessionFacts {
            port: 5000,
            max_message_size: None,
            unlimited_message_size: false,
            local_dtls_role: role,
        }
    }

    fn at(now: Instant, _start: Instant) -> Instant {
        now
    }

    fn drive(
        left: &mut Association,
        right: &mut Association,
        now: &mut Instant,
        start: Instant,
        left_events: &mut Vec<Event>,
        right_events: &mut Vec<Event>,
    ) {
        for _ in 0..500 {
            let mut progress = false;
            while let Some(packet) = left.poll_packet() {
                right.handle_input(at(*now, start), &packet);
                progress = true;
            }
            while let Some(packet) = right.poll_packet() {
                left.handle_input(at(*now, start), &packet);
                progress = true;
            }
            while let Some(event) = left.poll_event(at(*now, start)) {
                left_events.push(event);
                progress = true;
            }
            while let Some(event) = right.poll_event(at(*now, start)) {
                right_events.push(event);
                progress = true;
            }
            *now = now
                .checked_add(Duration::from_millis(10))
                .expect("test time");
            progress |= left.has_pending_events() || right.has_pending_events();
            if !progress
                && left.socket.state() == SocketState::Connected
                && right.socket.state() == SocketState::Connected
            {
                break;
            }
        }
    }

    fn connected_pair(
        left_limits: ConnectionLimits,
        right_limits: ConnectionLimits,
    ) -> (Association, Association, Instant, Instant) {
        connected_pair_with_facts(
            left_limits,
            right_limits,
            facts(DtlsRole::Active),
            facts(DtlsRole::Passive),
        )
    }

    fn connected_pair_with_facts(
        left_limits: ConnectionLimits,
        right_limits: ConnectionLimits,
        left_facts: SctpSessionFacts,
        right_facts: SctpSessionFacts,
    ) -> (Association, Association, Instant, Instant) {
        let start = Instant::now();
        let mut now = start;
        let mut left = Association::new(left_facts, left_limits, start).unwrap();
        let mut right = Association::new(right_facts, right_limits, start).unwrap();
        left.connect(at(now, start));
        right.connect(at(now, start));
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert_eq!(left.socket.state(), SocketState::Connected);
        assert_eq!(right.socket.state(), SocketState::Connected);
        (left, right, now, start)
    }

    fn opened_channel(events: &[Event]) -> Option<DataChannelId> {
        events.iter().find_map(|event| match event {
            Event::DataChannel(DataChannelEvent::Opened { channel, .. }) => Some(*channel),
            _ => None,
        })
    }

    #[test]
    fn data_channel_admission_uses_actual_association_stream_capacity() {
        let wire = u32::MAX as usize;
        let left_limits = ConnectionLimits {
            max_inbound_data_message_bytes: wire - SCTP_MTU,
            max_buffered_data_bytes: wire - MAX_DCEP_MESSAGE_BYTES,
            ..ConnectionLimits::default()
        };
        let right_limits = ConnectionLimits::default();
        let start = Instant::now();
        assert_eq!(left_limits.validate(), Ok(left_limits));
        let buffers = buffer_limits(left_limits).unwrap();
        assert_eq!(buffers.receive_window_bytes, wire);
        assert_eq!(buffers.send_buffer_bytes, wire);
        for invalid in [
            ConnectionLimits {
                max_inbound_data_message_bytes: left_limits.max_inbound_data_message_bytes + 1,
                ..left_limits
            },
            ConnectionLimits {
                max_buffered_data_bytes: left_limits.max_buffered_data_bytes + 1,
                ..left_limits
            },
            ConnectionLimits {
                max_inbound_data_message_bytes: usize::MAX,
                ..left_limits
            },
            ConnectionLimits {
                max_buffered_data_bytes: usize::MAX,
                ..left_limits
            },
        ] {
            assert_eq!(invalid.validate(), Err(AcceptError::InvalidConfiguration));
            assert_eq!(
                Association::new(facts(DtlsRole::Active), invalid, start).err(),
                Some(AcceptError::InvalidConfiguration)
            );
        }
        let mut now = start;
        let mut left = Association::new(facts(DtlsRole::Active), left_limits, start).unwrap();
        let mut right = Association::new(facts(DtlsRole::Passive), right_limits, start).unwrap();
        right.socket = Socket::new(
            "bounded-peer",
            &Options {
                announced_maximum_incoming_streams: 16,
                announced_maximum_outgoing_streams: 8,
                ..Options::default()
            },
        );
        let zero = DataChannelId::from_value(0);
        let mut declared = config(DataReliability::Reliable, true);
        declared.id = Some(zero);
        declared.negotiated = true;
        let lifetime = left.next_channel_lifetime;
        left.max_runtime_bytes = 0;
        let (reason, returned) = left.open(now, declared.clone()).unwrap_err();
        assert_eq!(reason, WriteError::WouldBlock);
        assert_eq!(returned, declared);
        assert_eq!(left.next_channel_lifetime, lifetime);
        assert_eq!(left.test_runtime_bytes(), 0);
        left.max_runtime_bytes = left_limits.max_data_runtime_bytes;
        left.open(now, declared.clone())
            .expect("fund declaration before readiness");
        right
            .open(now, declared.clone())
            .expect("peer declares the same stream");
        assert_eq!(left.stream_capacity(), None);
        assert!(left.poll_event(now).is_none());
        left.handle_dcep(StreamId(0), &[DCEP_ACK]);
        assert!(
            left.poll_event(now).is_none(),
            "DCEP cannot activate a declaration"
        );
        assert!(
            left.channel(StreamId(0))
                .is_some_and(|channel| channel.state == ChannelState::Opening)
        );
        assert_eq!(left.future_events, usize::from(CHANNEL_NOTIFICATIONS));
        assert!(left.test_runtime_bytes() > 0);
        assert!(left.outgoing.is_empty());
        declared.id = Some(DataChannelId::from_value(8));
        left.open(now, declared.clone())
            .expect("peer capacity is not known yet");
        declared.id = Some(DataChannelId::from_value(9));
        left.open(now, declared.clone())
            .expect("pending cancellation");
        assert_eq!(
            left.close_channel(now, DataChannelId::from_value(9)),
            Err(WriteError::InvalidState)
        );
        assert!(left.channel(StreamId(9)).is_none());
        assert!(
            left.resetting
                .get(&StreamId(9))
                .is_some_and(|reset| reset.failed)
        );
        assert_eq!(
            left.open(now, declared.clone())
                .map_err(|(reason, _)| reason),
            Err(WriteError::InvalidState)
        );
        declared.id = Some(DataChannelId::from_value(u16::MAX));
        assert_eq!(
            left.open(now, declared).map_err(|(reason, _)| reason),
            Err(WriteError::InvalidState)
        );
        let mut left_events = Vec::new();
        let mut right_events = Vec::new();
        left.connect(now);
        assert_eq!(
            left.outgoing.front().unwrap().get(20..24),
            Some(u32::MAX.to_be_bytes().as_slice()),
            "native INIT advertises the exact full-width receive window without allocating its capacity"
        );
        let init = left.poll_packet().expect("INIT");
        right.handle_input(now, &init);
        let init_ack = right.poll_packet().expect("INIT ACK");
        left.handle_input(now, &init_ack);
        let cookie_echo = left.poll_packet().expect("COOKIE ECHO");
        right.handle_input(now, &cookie_echo);
        let cookie_ack = right.poll_packet().expect("COOKIE ACK");
        assert_eq!(cookie_ack[12], 11);
        assert_eq!(left.socket.state(), SocketState::Connecting);
        assert_eq!(right.socket.state(), SocketState::Connected);
        right
            .send(
                now,
                zero,
                DataMessage::Binary(Bytes::from_static(b"first control")),
            )
            .expect("peer DATA before COOKIE ACK");
        let data = right.poll_packet().expect("first peer DATA");
        left.handle_input(now, &data);
        assert_eq!(left.socket.state(), SocketState::Connecting);
        assert_eq!(left.stream_capacity(), None);
        while let Some(event) = left.poll_event(now) {
            assert!(!matches!(
                event,
                Event::DataChannel(
                    DataChannelEvent::Opened { .. } | DataChannelEvent::Message { .. }
                )
            ));
            left_events.push(event);
        }
        assert_eq!(left.queued_inbound_payload_bytes, 0);
        left.handle_input(now, &cookie_ack);
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert_eq!(opened_channel(&left_events), Some(zero));
        assert_eq!(opened_channel(&right_events), Some(zero));
        assert!(left.channel(StreamId(8)).is_none());
        assert!(
            left.resetting
                .get(&StreamId(8))
                .is_some_and(|reset| reset.failed)
        );
        assert!(!left_events.iter().any(|event| matches!(event, Event::DataChannel(DataChannelEvent::Opened { channel, .. }) if *channel != zero)));
        let opened = left_events.iter().position(|event| matches!(event, Event::DataChannel(DataChannelEvent::Opened { channel, .. }) if *channel == zero)).unwrap();
        let message = left_events.iter().position(|event| matches!(event, Event::DataChannel(DataChannelEvent::Message { channel, message: DataMessage::Binary(bytes) }) if *channel == zero && bytes.as_ref() == b"first control")).unwrap();
        assert!(opened < message);
        left.close_channel(now, zero)
            .expect("reset active declaration");
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert!(left.channels.is_empty());
        assert!(right.channels.is_empty());
        assert_eq!(left.config_runtime_bytes, 0);
        assert_eq!(left.test_reset_runtime_bytes(), 2 * RESET_BYTES);
        assert_eq!(left.test_runtime_bytes(), 2 * RESET_BYTES);
        assert_eq!(left.stream_capacity(), Some(8));
        assert_eq!(right.stream_capacity(), Some(8));
        let metrics = left.socket.get_metrics().unwrap();
        assert_eq!(metrics.negotiated_maximum_incoming_streams, 8);
        assert_eq!(metrics.negotiated_maximum_outgoing_streams, 16);
        let mut channel = config(DataReliability::Reliable, true);
        channel.negotiated = true;
        for invalid in [8, u16::MAX] {
            channel.id = Some(DataChannelId::from_value(invalid));
            assert_eq!(
                left.open(now, channel.clone()).map_err(|(error, _)| error),
                Err(WriteError::InvalidState)
            );
        }
        assert!(left.channels.is_empty());
        channel.id = Some(DataChannelId::from_value(7));
        let lifetime = left.next_channel_lifetime;
        left.max_runtime_bytes = 0;
        assert_eq!(
            left.open(now, channel.clone()).map_err(|(error, _)| error),
            Err(WriteError::WouldBlock)
        );
        assert!(left.channels.is_empty());
        assert_eq!(left.next_channel_lifetime, lifetime);
        left.max_runtime_bytes = left_limits.max_data_runtime_bytes;
        assert!(left.open(now, channel.clone()).is_ok());
        let mut extra = channel.clone();
        extra.id = Some(DataChannelId::from_value(6));
        left.open(now, extra)
            .expect("another funded SID remains available below negotiated capacity");
        left.reset_all_cursor = Some(StreamId(0));
        assert_eq!(
            left.send(
                now,
                DataChannelId::from_value(7),
                DataMessage::Binary(Bytes::from_static(b"fenced"))
            )
            .map_err(|(error, _)| error),
            Err(WriteError::InvalidState)
        );
        assert!(left.open(now, channel).is_err());
        left.visit_reset_all();
        assert!(left.channel(StreamId(6)).unwrap().state == ChannelState::Closing);
        assert!(left.channel(StreamId(7)).unwrap().state == ChannelState::Open);
        assert_eq!(left.reset_all_cursor, Some(StreamId(7)));
        left.visit_reset_all();
        assert!(left.channel(StreamId(7)).unwrap().state == ChannelState::Closing);
        assert_eq!(left.reset_all_cursor, Some(StreamId(8)));
        left.visit_reset_all();
        assert_eq!(left.reset_all_cursor, Some(StreamId(9)));
        left.visit_reset_all();
        assert_eq!(left.reset_all_cursor, Some(StreamId(10)));
        left.visit_reset_all();
        assert_eq!(left.reset_all_cursor, None);
        assert!(left.resetting.get(&StreamId(8)).unwrap().failed);
        assert!(left.resetting.get(&StreamId(9)).unwrap().failed);
    }

    #[test]
    fn data_channel_open_events_expose_assigned_identity_and_complete_configuration() {
        for (reliability, ordered, negotiated) in [
            (DataReliability::Reliable, true, false),
            (DataReliability::MaxRetransmits(3), false, false),
            (
                DataReliability::MaxLifetime(Duration::from_millis(90)),
                true,
                false,
            ),
            (DataReliability::Reliable, true, true),
        ] {
            let (mut left, mut right, mut now, start) = connected_pair_with_facts(
                ConnectionLimits::default(),
                ConnectionLimits::default(),
                SctpSessionFacts {
                    max_message_size: Some(MAX_DCEP_MESSAGE_BYTES),
                    ..facts(DtlsRole::Active)
                },
                facts(DtlsRole::Passive),
            );
            let mut expected = config(reliability, ordered);
            expected.negotiated = negotiated;
            if reliability == DataReliability::Reliable {
                expected.label = Arc::from(format!("{}a", "é".repeat(32_767)));
                expected.protocol = Arc::from(format!("{}abc", "🦀".repeat(16_383)));
                if !negotiated {
                    let before = (
                        left.next_channel_lifetime,
                        left.test_runtime_bytes(),
                        left.events.capacity(),
                        left.socket.get_stream_priority(StreamId(0)),
                    );
                    for (label, protocol) in [
                        (
                            Arc::from("x".repeat(usize::from(u16::MAX) + 1)),
                            Arc::from(""),
                        ),
                        (
                            Arc::from(""),
                            Arc::from("x".repeat(usize::from(u16::MAX) + 1)),
                        ),
                    ] {
                        let invalid = DataChannelConfig {
                            label,
                            protocol,
                            ..expected.clone()
                        };
                        let original = invalid.clone();
                        let (reason, returned) = left.open(at(now, start), invalid).unwrap_err();
                        assert_eq!(reason, WriteError::InvalidState);
                        assert_eq!(returned, original);
                        assert_eq!(returned.label.as_ptr(), original.label.as_ptr());
                        assert_eq!(returned.protocol.as_ptr(), original.protocol.as_ptr());
                        assert_eq!(
                            (
                                left.next_channel_lifetime,
                                left.test_runtime_bytes(),
                                left.events.capacity(),
                                left.socket.get_stream_priority(StreamId(0))
                            ),
                            before
                        );
                    }
                    let before_right = (
                        right.next_channel_lifetime,
                        right.test_runtime_bytes(),
                        right.events.capacity(),
                    );
                    let (reason, returned) =
                        right.open(at(now, start), expected.clone()).unwrap_err();
                    assert_eq!(
                        reason,
                        WriteError::InvalidState,
                        "absent peer limit still means 64 KiB, not the larger DCEP format maximum"
                    );
                    assert_eq!(returned, expected);
                    assert_eq!(
                        (
                            right.next_channel_lifetime,
                            right.test_runtime_bytes(),
                            right.events.capacity()
                        ),
                        before_right
                    );
                    assert!(right.outgoing.is_empty());
                    let (config_bytes, events) = left.prepare_channel(&expected).unwrap();
                    let exact = CHANNEL_BYTES + RESET_BYTES + config_bytes + events.bytes();
                    drop(events);
                    left.max_runtime_bytes = exact - 1;
                    let (reason, returned) =
                        left.open(at(now, start), expected.clone()).unwrap_err();
                    assert_eq!(reason, WriteError::WouldBlock);
                    assert_eq!(returned, expected);
                    assert_eq!(returned.label.as_ptr(), expected.label.as_ptr());
                    assert_eq!(returned.protocol.as_ptr(), expected.protocol.as_ptr());
                    assert_eq!(
                        (
                            left.next_channel_lifetime,
                            left.test_runtime_bytes(),
                            left.events.capacity(),
                            left.socket.get_stream_priority(StreamId(0))
                        ),
                        before
                    );
                    assert!(left.outgoing.is_empty());
                    left.max_runtime_bytes = exact;
                }
            }
            if negotiated {
                expected.id = Some(DataChannelId::from_value(7));
                right.open(at(now, start), expected.clone()).unwrap();
            }
            left.open(at(now, start), expected.clone()).unwrap();
            let mut left_events = Vec::new();
            let mut right_events = Vec::new();
            drive(
                &mut left,
                &mut right,
                &mut now,
                start,
                &mut left_events,
                &mut right_events,
            );
            let channel = opened_channel(&left_events).expect("local opened");
            expected.id = Some(channel);
            for events in [&left_events, &right_events] {
                let configurations: Vec<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        Event::DataChannel(DataChannelEvent::Opened {
                            channel: opened,
                            config,
                        }) => {
                            assert_eq!(*opened, channel);
                            Some(config)
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(configurations, vec![&expected]);
            }
        }
    }

    #[test]
    fn data_channel_associations_fragment_reassemble_deduplicate_dcep_and_reuse_streams() {
        let limits = ConnectionLimits::default();
        let (mut left, mut right, mut now, start) = connected_pair_with_facts(
            limits,
            limits,
            SctpSessionFacts {
                unlimited_message_size: true,
                ..facts(DtlsRole::Active)
            },
            facts(DtlsRole::Passive),
        );
        assert_eq!(
            left.max_outbound_message_bytes,
            limits.max_buffered_data_bytes
        );
        assert_eq!(right.peer_max_message_bytes, 65_536);
        assert_eq!(right.socket.options().max_message_size, 65_536);
        right
            .socket
            .send(
                Message::new(StreamId(0), DCEP_PPID, vec![DCEP_ACK]),
                &SendOptions::default(),
            )
            .expect("early DCEP ACK");
        right.drain_socket();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        left.open(at(now, start), config(DataReliability::Reliable, true))
            .expect("open channel");
        let mut left_events = Vec::new();
        let mut right_events = Vec::new();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        let channel = opened_channel(&left_events).expect("local opened");
        assert_eq!(opened_channel(&right_events), Some(channel));
        let current = left.test_runtime_bytes();
        left.max_runtime_bytes = current;
        let lifetime = left.next_channel_lifetime;
        let extra = config(DataReliability::Reliable, true);
        let label = extra.label.as_ptr();
        let (reason, returned) = left.open(at(now, start), extra).unwrap_err();
        assert_eq!(reason, WriteError::WouldBlock);
        assert_eq!(returned.label.as_ptr(), label);
        assert_eq!(left.next_channel_lifetime, lifetime);
        assert_eq!(left.test_runtime_bytes(), current);
        left.max_runtime_bytes = limits.max_data_runtime_bytes;

        let stream = StreamId(channel.value());
        let duplicate = encode_dcep_open(&left.channel(stream).expect("channel").config);
        left.socket
            .send(
                Message::new(stream, DCEP_PPID, duplicate),
                &SendOptions::default(),
            )
            .expect("duplicate DCEP");
        left.drain_socket();
        left.send(
            at(now, start),
            channel,
            DataMessage::Binary(Bytes::from(vec![9; 65_537])),
        )
        .expect("fragmented message");
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert_eq!(
            right_events
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::DataChannel(DataChannelEvent::Opened { .. })
                ))
                .count(),
            1
        );
        assert!(right_events.iter().any(|event| matches!(
            event,
            Event::DataChannel(DataChannelEvent::Message {
                message: DataMessage::Binary(bytes),
                ..
            }) if bytes.len() == 65_537
        )));

        left.close_channel(at(now, start), channel).expect("close");
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert!(left.channels.is_empty());
        left.open(
            at(now, start),
            config(
                DataReliability::MaxLifetime(Duration::from_millis(10)),
                false,
            ),
        )
        .expect("reuse released stream");
        assert!(left.channels.contains_key(&stream));
        let live = usize::from(ConnectionLimits::DEFAULT_MAX_DATA_CHANNELS) + 1;
        for _ in 1..live {
            left.open(at(now, start), config(DataReliability::Reliable, true))
                .expect("funded local admission beyond the former default count");
        }
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        for owner in [&left, &right] {
            assert_eq!(owner.channels.len(), live);
            assert!(
                owner
                    .channels
                    .values()
                    .all(|channel| channel.state == ChannelState::Open)
            );
            assert_eq!(owner.test_reset_runtime_bytes(), live * RESET_BYTES);
            assert_eq!(owner.future_events, live * 2);
            owner.test_runtime_bytes();
        }
        let lifetime = right.next_channel_lifetime;
        right.max_runtime_bytes = right.test_runtime_bytes() + RESET_BYTES;
        let rejected_stream = left.allocate_stream().unwrap();
        left.open(at(now, start), config(DataReliability::Reliable, true))
            .expect("remote exhausted-credit admission attempt");
        let packet = left
            .poll_packet()
            .expect("real DCEP packet for denied stream");
        right.handle_input(at(now, start), &packet);
        assert!(right.resetting.contains_key(&rejected_stream));
        assert!(!right.channels.contains_key(&rejected_stream));
        assert_eq!(right.test_reset_runtime_bytes(), (live + 1) * RESET_BYTES);
        assert_eq!(right.next_channel_lifetime, lifetime);
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert_eq!(right.next_channel_lifetime, lifetime);
        assert_eq!(left.channels.len(), live);
        assert_eq!(right.channels.len(), live);
        assert!(!left.stopped);
        assert!(!right.stopped);
        right.max_runtime_bytes = limits.max_data_runtime_bytes;
        left.send(
            at(now, start),
            DataChannelId::from_value(2),
            DataMessage::Binary(Bytes::from_static(b"healthy")),
        )
        .unwrap();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert!(right_events.iter().any(|event| matches!(event, Event::DataChannel(DataChannelEvent::Message { channel, message: DataMessage::Binary(bytes) }) if channel.value() == 2 && bytes.as_ref() == b"healthy")));
        left.test_runtime_bytes();
        right.test_runtime_bytes();
    }

    #[test]
    fn data_channel_reuse_waits_for_both_resets_and_discards_old_lifetime_events() {
        let limits = ConnectionLimits::default();
        let (mut left, mut right, mut now, start) = connected_pair(limits, limits);
        let id = DataChannelId::from_value(0);
        let stream = StreamId(id.value());
        let mut channel = config(DataReliability::Reliable, true);
        channel.negotiated = true;
        channel.id = Some(id);
        left.open(now, channel.clone()).unwrap();
        right.open(now, channel.clone()).unwrap();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        let old_lifetime = left.channel(stream).unwrap().lifetime;
        right
            .send(now, id, DataMessage::Binary(Bytes::from_static(b"old")))
            .unwrap();
        while let Some(packet) = right.poll_packet() {
            left.handle_input(now, &packet);
        }
        assert!(
            left.events
                .iter()
                .any(|queued| matches!(queued.event, DataChannelEvent::Message { .. }))
        );
        let queued = left
            .events
            .iter()
            .map(|event| event.retained_payload_bytes)
            .sum::<usize>();
        assert!(queued >= b"old".len());
        assert_eq!(left.queued_inbound_payload_bytes, queued);
        assert_eq!(left.test_reset_runtime_bytes(), RESET_BYTES);
        left.max_runtime_bytes = left.test_runtime_bytes();
        left.close_channel(now, id).unwrap();
        assert_eq!(left.test_reset_runtime_bytes(), RESET_BYTES);
        assert_eq!(left.queued_inbound_payload_bytes, queued);
        assert_eq!(
            left.send(now, id, DataMessage::Binary(Bytes::from_static(b"blocked")))
                .map_err(|(error, _)| error),
            Err(WriteError::InvalidState)
        );
        assert_eq!(
            left.open(now, channel.clone()),
            Err((WriteError::ResetPending, channel.clone()))
        );
        // Outgoing reset completion is not an incoming reset barrier.
        left.resetting.get_mut(&stream).unwrap().outgoing = true;
        left.queue_reset_completion(stream);
        assert!(left.ready_resets.is_empty());
        assert_eq!(
            left.open(now, channel.clone()),
            Err((WriteError::ResetPending, channel.clone()))
        );
        assert_eq!(left.test_reset_runtime_bytes(), RESET_BYTES);
        let mut left_events = Vec::new();
        let mut right_events = Vec::new();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert!(
            left_events.iter().all(|event| !matches!(
                event,
                Event::DataChannel(DataChannelEvent::Message { .. })
            ))
        );
        for events in [&left_events, &right_events] {
            assert!(events.iter().any(|event| matches!(event, Event::DataChannel(DataChannelEvent::ResetComplete { channel }) if *channel == id)));
        }
        assert!(left.channels.is_empty() && right.channels.is_empty());
        assert!(left.resetting.is_empty() && right.resetting.is_empty());
        assert_eq!(left.test_reset_runtime_bytes(), 0);
        assert_eq!(right.test_reset_runtime_bytes(), 0);
        assert_eq!(left.queued_inbound_payload_bytes, 0);
        left.open(now, channel.clone()).unwrap();
        right.open(now, channel).unwrap();
        left.max_runtime_bytes += 4 * EVENT_BYTES;
        left.prepare_events(0, 4).unwrap().commit(&mut left.events);
        let stale = b"stale".to_vec();
        let stale_capacity = stale.capacity();
        for event in [
            DataChannelEvent::Message {
                channel: id,
                message: DataMessage::Binary(Bytes::from(stale)),
            },
            DataChannelEvent::Closed { channel: id },
            DataChannelEvent::ResetComplete { channel: id },
        ] {
            let retained_payload_bytes = if matches!(event, DataChannelEvent::Message { .. }) {
                stale_capacity
            } else {
                0
            };
            left.queued_inbound_payload_bytes += retained_payload_bytes;
            left.events.push_back(QueuedEvent {
                lifetime: old_lifetime,
                event,
                retained_payload_bytes,
                config_lease: None,
            });
        }
        assert_eq!(left.queued_inbound_payload_bytes, stale_capacity);
        right
            .send(now, id, DataMessage::Binary(Bytes::from_static(b"new")))
            .unwrap();
        left_events.clear();
        right_events.clear();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert!(left_events.iter().any(|event| matches!(event, Event::DataChannel(DataChannelEvent::Message { message: DataMessage::Binary(bytes), .. }) if bytes.as_ref() == b"new")));
        assert!(left_events.iter().all(|event| !matches!(
            event,
            Event::DataChannel(
                DataChannelEvent::Closed { .. } | DataChannelEvent::ResetComplete { .. }
            )
        )));
        assert!(left_events.iter().all(|event| !matches!(event, Event::DataChannel(DataChannelEvent::Message { message: DataMessage::Binary(bytes), .. }) if bytes.as_ref() == b"stale")));
        assert_eq!(left.queued_inbound_payload_bytes, 0);
        let unknown = DataChannelId::from_value(4);
        let mut unpublished = config(DataReliability::Reliable, true);
        unpublished.id = Some(unknown);
        unpublished.negotiated = true;
        right.open(now, unpublished.clone()).unwrap();
        right.close_channel(now, unknown).unwrap();
        left.max_runtime_bytes += RESET_BYTES + 4 * EVENT_BYTES;
        while let Some(packet) = right.poll_packet() {
            left.handle_input(now, &packet);
        }
        assert!(!left.channels.contains_key(&StreamId(unknown.value())));
        assert_eq!(
            left.open(now, unpublished.clone()),
            Err((WriteError::ResetPending, unpublished))
        );
        left_events.clear();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert!(!left.resetting.contains_key(&StreamId(unknown.value())));
        assert!(left_events.iter().any(|event| matches!(event, Event::DataChannel(DataChannelEvent::ResetComplete { channel }) if *channel == unknown)));
        assert!(left.channels[&stream].state == ChannelState::Open);
        assert_eq!(left.test_reset_runtime_bytes(), RESET_BYTES);
    }

    #[test]
    fn data_channel_failed_reset_and_exhausted_counters_never_reuse_a_lifetime() {
        let (mut left, mut right, mut now, start) =
            connected_pair(ConnectionLimits::default(), ConnectionLimits::default());
        let id = DataChannelId::from_value(0);
        let stream = StreamId(id.value());
        let mut channel = config(DataReliability::Reliable, true);
        channel.id = Some(id);
        channel.negotiated = true;
        left.open(now, channel.clone()).unwrap();
        right.open(now, channel.clone()).unwrap();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        left.next_lifecycle = u64::MAX;
        left.send(now, id, DataMessage::Binary(Bytes::from_static(b"last")))
            .unwrap();
        assert_eq!(left.next_lifecycle, 0);
        assert_eq!(
            left.send(now, id, DataMessage::Binary(Bytes::from_static(b"no-wrap")))
                .map_err(|(error, _)| error),
            Err(WriteError::WouldBlock)
        );
        left.close_channel(now, id).unwrap();
        left.fail_reset(stream);
        let mut events = Vec::new();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut events,
            &mut Vec::new(),
        );
        assert!(left.resetting.get(&stream).unwrap().failed);
        assert_eq!(
            left.open(now, channel.clone()),
            Err((WriteError::InvalidState, channel))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            Event::DataChannel(DataChannelEvent::ResetFailed { .. })
        )));
        assert!(events.iter().all(|event| !matches!(
            event,
            Event::DataChannel(DataChannelEvent::ResetComplete { .. })
        )));
        assert_eq!(left.test_reset_runtime_bytes(), RESET_BYTES);
        let mut fresh = config(DataReliability::Reliable, true);
        fresh.negotiated = true;
        fresh.id = Some(DataChannelId::from_value(2));
        let storage = fresh.label.as_ptr();
        let lifetime = left.next_channel_lifetime;
        left.max_runtime_bytes = left.test_runtime_bytes()
            + CHANNEL_BYTES
            + RESET_BYTES
            + std::mem::size_of::<ConfigLease>()
            + fresh.label.len()
            + fresh.protocol.len()
            + usize::from(CHANNEL_NOTIFICATIONS) * EVENT_BYTES
            - 1;
        let (reason, fresh) = left
            .open(now, fresh)
            .expect_err("new channel, config and reset promise are one byte short");
        assert_eq!(reason, WriteError::WouldBlock);
        assert_eq!(fresh.label.as_ptr(), storage);
        assert_eq!(left.next_channel_lifetime, lifetime);
        assert_eq!(left.test_reset_runtime_bytes(), RESET_BYTES);
        left.max_runtime_bytes += 1;
        left.open(now, fresh).expect("exact prepaid reset promise");
        assert_eq!(left.test_reset_runtime_bytes(), 2 * RESET_BYTES);
        left.close_channel(now, DataChannelId::from_value(2))
            .expect("cutoff needs no free bytes");
        assert_eq!(left.test_reset_runtime_bytes(), 2 * RESET_BYTES);
        for value in [2, 6] {
            let mut fresh = config(DataReliability::Reliable, true);
            fresh.id = Some(DataChannelId::from_value(value));
            fresh.negotiated = true;
            right.open(now, fresh).unwrap();
        }
        let config_bytes = right.config_runtime_bytes;
        right.max_runtime_bytes = right.test_runtime_bytes();
        let available = right.events.capacity() - right.events.len() - right.future_events;
        for _ in 0..available {
            right.handle_message(Message::new(StreamId(2), EMPTY_BINARY_PPID, vec![0]));
            assert!(right.channels[&StreamId(2)].state == ChannelState::Open);
        }
        let capacity = right.events.capacity();
        right.handle_message(Message::new(StreamId(2), EMPTY_BINARY_PPID, vec![0]));
        assert!(right.channels[&StreamId(2)].state == ChannelState::Closing);
        assert!(right.channels[&StreamId(6)].state == ChannelState::Open);
        assert!(
            !right.stopped,
            "event exhaustion resets only the offending stream"
        );
        assert_eq!(right.events.capacity(), capacity);
        right.test_reset_runtime_bytes();
        for _ in 0..MAX_WORK_PER_CALL {
            while let Some(packet) = left.poll_packet() {
                right.handle_input(now, &packet);
            }
            while let Some(packet) = right.poll_packet() {
                left.handle_input(now, &packet);
            }
            left.advance_time(now);
            left.drain_socket();
            right.advance_time(now);
            right.drain_socket();
            if !right.channels.contains_key(&StreamId(2))
                && !left.channels.contains_key(&StreamId(2))
            {
                break;
            }
            now = now.checked_add(Duration::from_millis(10)).unwrap();
        }
        assert!(!right.channels.contains_key(&StreamId(2)));
        assert!(!left.channels.contains_key(&StreamId(2)));
        assert_eq!(
            right.config_runtime_bytes, config_bytes,
            "queued Opened retains the shared strings after native reset"
        );
        let mut cutoffs = Vec::new();
        while let Some(event) = right.poll_event(now) {
            cutoffs.push(event);
        }
        assert!(cutoffs.iter().all(|event| !matches!(event, Event::DataChannel(DataChannelEvent::Message { channel, .. }) if channel.value() == 2)));
        assert!(cutoffs.iter().any(|event| matches!(event, Event::DataChannel(DataChannelEvent::ResetComplete { channel }) if channel.value() == 2)));
        assert!(cutoffs.iter().all(|event| !matches!(event, Event::DataChannel(DataChannelEvent::Closed { channel }) if channel.value() == 6)));
        assert_eq!(
            right.config_runtime_bytes,
            right.channels[&StreamId(6)].config_lease.bytes
        );
        right
            .send(
                now,
                DataChannelId::from_value(6),
                DataMessage::Binary(Bytes::new()),
            )
            .expect("healthy stream still sends after offending queue reset");
        let records =
            usize::from(ConnectionLimits::DEFAULT_MAX_DATA_CHANNELS) * 2 + MAX_WORK_PER_CALL + 1;
        right.max_runtime_bytes = right.test_runtime_bytes() + records * EVENT_BYTES;
        right
            .prepare_events(0, records)
            .unwrap()
            .commit(&mut right.events);
        for _ in 0..records {
            right.handle_message(Message::new(StreamId(6), EMPTY_BINARY_PPID, vec![0]));
        }
        assert_eq!(
            right.events.len(),
            records,
            "funded event storage exceeds the former derived count ceiling"
        );
        assert!(right.channels[&StreamId(6)].state == ChannelState::Open);
        right.test_runtime_bytes();
        for _ in 0..records {
            assert!(
                matches!(right.poll_event(now), Some(Event::DataChannel(DataChannelEvent::Message { channel, message: DataMessage::Binary(bytes) })) if channel.value() == 6 && bytes.is_empty())
            );
        }
        right.test_runtime_bytes();
        assert_eq!(left.test_reset_runtime_bytes(), RESET_BYTES);
        let unknown = StreamId(4);
        left.max_runtime_bytes = left.test_runtime_bytes() + RESET_BYTES + 4 * EVENT_BYTES;
        left.incoming_reset(unknown);
        assert!(!left.channels.contains_key(&unknown));
        assert!(left.resetting[&unknown].incoming);
        assert_eq!(left.test_reset_runtime_bytes(), 2 * RESET_BYTES);
        let credited = left.test_runtime_bytes();
        left.incoming_reset(unknown);
        assert_eq!(left.test_reset_runtime_bytes(), 2 * RESET_BYTES);
        assert_eq!(left.test_runtime_bytes(), credited);
        left.fail_reset(unknown);
        let mut failed = Vec::new();
        while let Some(event) = left.poll_event(now) {
            failed.push(event);
        }
        assert_eq!(failed.iter().filter(|event| matches!(event, Event::DataChannel(DataChannelEvent::ResetFailed { channel }) if channel.value() == unknown.0)).count(), 1);
        left.fail_reset(unknown);
        assert!(left.poll_event(now).is_none());
        assert!(left.resetting[&unknown].failed);
        assert_eq!(left.test_reset_runtime_bytes(), 2 * RESET_BYTES);
        left.max_runtime_bytes = left.test_runtime_bytes();
        left.incoming_reset(StreamId(6));
        assert!(
            left.stopped,
            "cannot retain an unknown reset barrier at full capacity"
        );
        assert!(!left.resetting.contains_key(&StreamId(6)));
        assert_eq!(left.test_reset_runtime_bytes(), 2 * RESET_BYTES);
        assert_eq!(left.begin_reset(StreamId(8)), Err(WriteError::InvalidState));
        assert!(!left.resetting.contains_key(&StreamId(8)));
        left.abort();
        assert_eq!(left.test_reset_runtime_bytes(), 0);
        let lifetime = left.next_channel_lifetime;
        left.handle_message(Message::new(
            StreamId(1),
            DCEP_PPID,
            encode_dcep_open(&config(DataReliability::Reliable, true)),
        ));
        assert_eq!(left.next_channel_lifetime, lifetime);
        assert!(left.channels.is_empty());
        assert_eq!(left.test_reset_runtime_bytes(), 0);
        left.next_channel_lifetime = u64::MAX;
        assert_eq!(left.allocate_channel_lifetime(), Ok(u64::MAX));
        assert_eq!(
            left.allocate_channel_lifetime(),
            Err(WriteError::WouldBlock)
        );
        right.incoming_reset(StreamId(8));
        assert!(right.resetting[&StreamId(8)].notification_reserved);
        right.test_reset_runtime_bytes();
        right.abort();
        assert_eq!(right.future_events, 0);
        assert_eq!(right.events.capacity(), 0);
        assert_eq!(right.test_runtime_bytes(), 0);
    }

    #[test]
    fn data_channel_negotiated_stream_preserves_empty_message_boundaries() {
        let limits = ConnectionLimits {
            max_inbound_data_message_bytes: 1,
            max_buffered_data_bytes: 2,
            ..ConnectionLimits::default()
        };
        let (mut left, mut right, mut now, start) = connected_pair_with_facts(
            limits,
            limits,
            SctpSessionFacts {
                max_message_size: Some(1),
                ..facts(DtlsRole::Active)
            },
            SctpSessionFacts {
                unlimited_message_size: true,
                ..facts(DtlsRole::Passive)
            },
        );
        assert_eq!(left.peer_max_message_bytes, 1);
        assert_eq!(left.socket.options().max_message_size, 1);
        assert_eq!(right.peer_max_message_bytes, usize::MAX);
        assert_eq!(right.max_outbound_message_bytes, 2);
        assert_eq!(
            left.socket.options().max_receiver_window_buffer_size,
            MAX_DCEP_MESSAGE_BYTES + SCTP_MTU
        );
        let before = (
            left.next_channel_lifetime,
            left.next_lifecycle,
            left.test_runtime_bytes(),
            left.events.capacity(),
            left.socket.get_stream_priority(StreamId(0)),
        );
        let mut rejected = config(DataReliability::Reliable, true);
        rejected.priority = DataChannelPriority::HIGH;
        let label = rejected.label.as_ptr();
        let expected = rejected.clone();
        let (reason, returned) = left.open(at(now, start), rejected).unwrap_err();
        assert_eq!(reason, WriteError::InvalidState);
        assert_eq!(returned, expected);
        assert_eq!(returned.label.as_ptr(), label);
        assert_eq!(
            (
                left.next_channel_lifetime,
                left.next_lifecycle,
                left.test_runtime_bytes(),
                left.events.capacity(),
                left.socket.get_stream_priority(StreamId(0))
            ),
            before
        );
        assert!(left.channels.is_empty());
        assert!(left.outgoing.is_empty());
        assert_eq!(left.socket.state(), SocketState::Connected);
        assert_eq!(left.stream_capacity(), Some(u16::MAX));
        assert_eq!(right.stream_capacity(), Some(u16::MAX));
        let id = DataChannelId::from_value(u16::MAX - 1);
        let mut negotiated = config(DataReliability::Reliable, true);
        negotiated.id = Some(id);
        negotiated.negotiated = true;
        left.open(at(now, start), negotiated.clone())
            .expect("left negotiated channel");
        right
            .open(at(now, start), negotiated)
            .expect("right negotiated channel");
        assert!(left.outgoing.is_empty());
        assert!(right.outgoing.is_empty());
        let mut left_events = Vec::new();
        let mut right_events = Vec::new();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        let oversized = Bytes::from_static(&[1, 2]);
        let before = (
            left.next_lifecycle,
            left.buffered_payload_bytes,
            left.test_runtime_bytes(),
        );
        let (reason, returned) = left
            .send(at(now, start), id, DataMessage::Binary(oversized.clone()))
            .unwrap_err();
        assert_eq!(reason, WriteError::MessageTooLarge);
        assert!(
            matches!(returned, DataMessage::Binary(bytes) if bytes.as_ptr() == oversized.as_ptr())
        );
        assert_eq!(
            (
                left.next_lifecycle,
                left.buffered_payload_bytes,
                left.test_runtime_bytes()
            ),
            before
        );
        for _ in 0..2 {
            left.send(
                at(now, start),
                id,
                DataMessage::Binary(Bytes::from_static(&[1])),
            )
            .unwrap();
        }
        assert_eq!(left.buffered_payload_bytes, limits.max_buffered_data_bytes);
        left.send(at(now, start), id, DataMessage::Text(Bytes::new()))
            .expect("empty message");
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert!(right_events.iter().any(|event| matches!(
            event,
            Event::DataChannel(DataChannelEvent::Message {
                channel,
                message: DataMessage::Text(bytes),
            }) if *channel == id && bytes.is_empty()
        )));
        assert_eq!(right_events.iter().filter(|event| matches!(event, Event::DataChannel(DataChannelEvent::Message { message: DataMessage::Binary(bytes), .. }) if bytes.as_ref() == [1])).count(), 2);
        right
            .send(
                at(now, start),
                id,
                DataMessage::Binary(Bytes::from_static(&[2])),
            )
            .unwrap();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert!(left_events.iter().any(|event| matches!(event, Event::DataChannel(DataChannelEvent::Message { channel, message: DataMessage::Binary(bytes) }) if *channel == id && bytes.as_ref() == [2])));
        assert_eq!(left.socket.state(), SocketState::Connected);
        assert_eq!(right.socket.state(), SocketState::Connected);
    }

    #[test]
    fn predeclared_activation_is_bounded_and_fenced_by_reset_all() {
        let start = Instant::now();
        let limits = ConnectionLimits::default();
        let mut left = Association::new(facts(DtlsRole::Active), limits, start).unwrap();
        let mut right = Association::new(facts(DtlsRole::Passive), limits, start).unwrap();
        for index in 0..=MAX_WORK_PER_CALL {
            let mut declared = config(DataReliability::Reliable, true);
            declared.id = Some(DataChannelId::from_value(u16::try_from(index).unwrap()));
            declared.negotiated = true;
            left.open(start, declared).unwrap();
        }
        left.connect(start);
        right.connect(start);
        let mut fenced = false;
        for _ in 0..32 {
            while let Some(packet) = left.poll_packet() {
                right.handle_input(start, &packet);
            }
            while let Some(packet) = right.poll_packet() {
                left.socket.handle_input(&packet);
                if !fenced && left.socket.state() == SocketState::Connected {
                    let last = StreamId(u16::try_from(MAX_WORK_PER_CALL).unwrap());
                    left.reset_all_cursor = Some(StreamId(0));
                    left.activate_declared(last);
                    assert!(
                        left.channel(last)
                            .is_some_and(|channel| channel.state == ChannelState::Opening)
                    );
                    let cursor = left.declared_cursor;
                    left.visit_declared();
                    assert_eq!(left.declared_cursor, cursor);
                    left.handle_message(Message::new(last, BINARY_PPID, b"fenced".to_vec()));
                    left.drain_socket();
                    assert!(!left.channels.is_empty(), "reset traversal must yield");
                    assert!(left.has_pending_events());
                    fenced = true;
                } else {
                    left.drain_socket();
                }
            }
            while let Some(event) = left.poll_event(start) {
                assert!(matches!(
                    event,
                    Event::DataChannel(
                        DataChannelEvent::Closed { .. } | DataChannelEvent::ResetFailed { .. }
                    )
                ));
            }
            if fenced && left.reset_all_cursor.is_none() && left.declared_cursor.is_none() {
                break;
            }
        }
        assert!(fenced);
        assert!(left.channels.is_empty());
        assert_eq!(left.config_runtime_bytes, 0);
        assert_eq!(
            left.test_reset_runtime_bytes(),
            (MAX_WORK_PER_CALL + 1) * RESET_BYTES
        );
        assert_eq!(
            left.test_runtime_bytes(),
            (MAX_WORK_PER_CALL + 1) * RESET_BYTES
        );
        left.abort();
        assert_eq!(left.test_runtime_bytes(), 0);
        let mut declared = config(DataReliability::Reliable, true);
        declared.id = Some(DataChannelId::from_value(0));
        declared.negotiated = true;
        let mut pending = Association::new(facts(DtlsRole::Active), limits, start).unwrap();
        pending.open(start, declared).unwrap();
        pending.begin_shutdown(start, start);
        assert!(pending.is_stopped());
        assert!(matches!(
            pending.poll_event(start),
            Some(Event::DataChannel(DataChannelEvent::Closed { .. }))
        ));
        assert!(pending.poll_event(start).is_none());
        pending.abort();
        assert_eq!(pending.test_runtime_bytes(), 0);
        assert_eq!(pending.declared_cursor, None);
    }

    #[test]
    fn capped_native_lifecycle_drain_remains_ready_without_user_events() {
        let limits = ConnectionLimits::default();
        let (mut left, mut right, mut now, start) = connected_pair(limits, limits);
        left.open(at(now, start), config(DataReliability::Reliable, true))
            .expect("open");
        let mut left_events = Vec::new();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut Vec::new(),
        );
        let channel = opened_channel(&left_events).expect("opened");
        let mut acknowledgements = Vec::new();
        for _ in 0..192 {
            left.send(
                at(now, start),
                channel,
                DataMessage::Binary(Bytes::from_static(&[1])),
            )
            .expect("bounded tiny message");
            while let Some(packet) = left.poll_packet() {
                right.handle_input(at(now, start), &packet);
                while right.poll_event(at(now, start)).is_some() {}
                while let Some(ack) = right.poll_packet() {
                    acknowledgements.push(ack);
                }
            }
        }
        let before = left.stats().buffered_payload_bytes;
        let cumulative = acknowledgements.pop().expect("cumulative acknowledgement");
        left.handle_input(at(now, start), &cumulative);
        assert!(
            left.native_drain_exhausted,
            "real cumulative ACK exhausts callback work"
        );
        assert!(left.has_pending_events());
        for _ in 0..8 {
            assert!(
                left.poll_event(at(now, start)).is_none(),
                "lifecycle work is not an application event"
            );
            if !left.has_pending_events() {
                break;
            }
        }
        assert!(
            !left.has_pending_events(),
            "bounded continuation makes progress"
        );
        assert!(left.stats().buffered_payload_bytes < before);
    }

    #[test]
    fn channel_queue_observation_distinguishes_unsent_from_in_flight() {
        let (mut left, mut right, mut now, start) =
            connected_pair(ConnectionLimits::default(), ConnectionLimits::default());
        left.open(
            at(now, start),
            config(DataReliability::MaxRetransmits(0), false),
        )
        .expect("open");
        let mut events = Vec::new();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut events,
            &mut Vec::new(),
        );
        let channel = opened_channel(&events).expect("opened");
        left.send(
            at(now, start),
            channel,
            DataMessage::Binary(Bytes::from_static(&[1; 48])),
        )
        .expect("fits the native send window");
        assert_eq!(left.channel_stats()[0].queued_payload_bytes, 0);
        assert_eq!(left.channel_stats()[0].buffered_amount, 48);
        left.send(
            at(now, start),
            channel,
            DataMessage::Binary(Bytes::from(vec![2; 65_536])),
        )
        .expect("bounded fragmented message");
        let queued = left.channel_stats()[0].queued_payload_bytes;
        assert!(queued > 0 && queued <= 65_536);
        assert_eq!(left.channel_stats()[0].buffered_amount, 65_584);
    }

    #[test]
    fn data_channel_admission_is_bounded_before_socket_ownership() {
        let limits = ConnectionLimits {
            max_buffered_data_bytes: 64,
            ..ConnectionLimits::default()
        };
        let (mut left, mut right, mut now, start) = connected_pair(limits, limits);
        left.open(
            at(now, start),
            config(DataReliability::MaxRetransmits(0), false),
        )
        .expect("open");
        let mut left_events = Vec::new();
        let mut right_events = Vec::new();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        let channel = opened_channel(&left_events).expect("opened");
        left.max_runtime_bytes = left.test_runtime_bytes() + LIFECYCLE_BYTES - 1;
        let payload = Bytes::from(vec![1; 48]);
        let storage = payload.as_ptr();
        let lifecycle = left.next_lifecycle;
        let packets = left.outgoing.len();
        let (reason, message) = left
            .send(at(now, start), channel, DataMessage::Binary(payload))
            .expect_err("one byte short of a lifecycle record");
        assert_eq!(reason, WriteError::WouldBlock);
        let DataMessage::Binary(payload) = &message else {
            panic!("message kind preserved");
        };
        assert_eq!(payload.as_ptr(), storage);
        assert_eq!(payload.as_ref(), &[1; 48]);
        assert_eq!(left.next_lifecycle, lifecycle);
        assert_eq!(left.outgoing.len(), packets);
        assert!(left.lifecycles.is_empty());
        assert_eq!(left.buffered_payload_bytes, 0);
        left.max_runtime_bytes += 1;
        left.send(at(now, start), channel, message)
            .expect("exact lifecycle storage");
        assert_eq!(left.lifecycles.len(), 1);
        left.max_runtime_bytes = left.test_runtime_bytes() + LIFECYCLE_BYTES;
        assert_eq!(
            left.send(
                at(now, start),
                channel,
                DataMessage::Binary(Bytes::from_static(&[2; 24])),
            )
            .map_err(|(reason, _)| reason),
            Err(WriteError::WouldBlock)
        );
        assert_eq!(left.stats().buffered_payload_bytes, 48);
        let lifecycle = left.next_lifecycle;
        left.max_runtime_bytes -= 1;
        let (reason, empty) = left
            .send(at(now, start), channel, DataMessage::Text(Bytes::new()))
            .expect_err("empty messages still need lifecycle storage");
        assert_eq!(reason, WriteError::WouldBlock);
        assert!(matches!(&empty, DataMessage::Text(bytes) if bytes.is_empty()));
        assert_eq!(left.next_lifecycle, lifecycle);
        assert_eq!(left.lifecycles.len(), 1);
        left.max_runtime_bytes += 1;
        left.send(at(now, start), channel, empty)
            .expect("exact second record allowance");
        assert_eq!(left.lifecycles.len(), 2);
        assert_eq!(left.buffered_payload_bytes, 48);
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        now = now
            .checked_add(Duration::from_secs(1))
            .expect("native timers");
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert!(left.lifecycles.is_empty());
        assert_eq!(left.buffered_payload_bytes, 0);
        left.max_runtime_bytes = left.test_runtime_bytes() + 4_097 * LIFECYCLE_BYTES;
        for _ in 0..4_097 {
            left.send(at(now, start), channel, DataMessage::Binary(Bytes::new()))
                .expect("funded records are not capped at the former 4096 limit");
            while left.poll_packet().is_some() {}
        }
        assert_eq!(left.lifecycles.len(), 4_097);
        assert_eq!(left.buffered_payload_bytes, 0);
        let lifecycle = left.next_lifecycle;
        assert_eq!(
            left.send(at(now, start), channel, DataMessage::Binary(Bytes::new()))
                .map_err(|(reason, _)| reason),
            Err(WriteError::WouldBlock)
        );
        assert_eq!(left.next_lifecycle, lifecycle);
        assert_eq!(left.lifecycles.len(), 4_097);
        left.abort();
        assert!(left.lifecycles.is_empty());
    }

    #[test]
    fn data_channel_oversized_input_isolated_and_shutdown_bounded() {
        let left_limits = ConnectionLimits {
            max_buffered_data_bytes: 64,
            ..ConnectionLimits::default()
        };
        let right_limits = ConnectionLimits {
            max_inbound_data_message_bytes: 4,
            max_buffered_data_bytes: 64,
            ..ConnectionLimits::default()
        };
        let (mut left, mut right, mut now, start) = connected_pair(left_limits, right_limits);
        for _ in 0..2 {
            left.open(at(now, start), config(DataReliability::Reliable, true))
                .expect("open");
        }
        let mut left_events = Vec::new();
        let mut right_events = Vec::new();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        let channels: Vec<_> = left_events
            .iter()
            .filter_map(|event| match event {
                Event::DataChannel(DataChannelEvent::Opened { channel, .. }) => Some(*channel),
                _ => None,
            })
            .collect();
        left.send(
            at(now, start),
            channels[1],
            DataMessage::Binary(Bytes::from_static(&[7; 4])),
        )
        .unwrap();
        let data_packet = left.poll_packet().expect("real native DATA packet");
        for _ in 0..MAX_WORK_PER_CALL * 2 {
            right.handle_input(now, &data_packet);
            if right.outgoing.len() > MAX_WORK_PER_CALL {
                break;
            }
        }
        assert!(!right.stopped);
        assert!(
            right.outgoing.len() > MAX_WORK_PER_CALL,
            "real duplicate DATA generates more queued native SACKs than the former packet ceiling"
        );
        right.test_packet_bytes();
        let mut acknowledgement = right.poll_packet().unwrap();
        acknowledgement.reserve(acknowledgement.capacity() + 64);
        let pointer = acknowledgement.as_ptr();
        let capacity = acknowledgement.capacity();
        let contents = acknowledgement.clone();
        right.clear_packets();
        right.max_packet_bytes = capacity + PACKET_BYTES - 1;
        let acknowledgement = right
            .queue_packet(acknowledgement)
            .expect_err("one byte short including packet capacity and deque backing");
        assert_eq!(acknowledgement.as_ptr(), pointer);
        assert_eq!(acknowledgement, contents);
        assert_eq!(right.test_packet_bytes(), 0);
        right.max_packet_bytes += 1;
        right.queue_packet(acknowledgement).unwrap();
        assert_eq!(right.test_packet_bytes(), right.max_packet_bytes);
        let held_packet = right.poll_packet().unwrap();
        assert_eq!(held_packet.as_ptr(), pointer);
        assert_eq!(right.outgoing_packet_bytes, 0);
        assert_eq!(
            right.test_packet_bytes(),
            PACKET_BYTES,
            "live channels reuse the funded deque backing, caller retains the packet"
        );
        right.max_packet_bytes = right_limits.max_transport_output_bytes;
        right.queue_packet(held_packet.clone()).unwrap();
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        left.send(
            at(now, start),
            channels[0],
            DataMessage::Binary(Bytes::from_static(&[1; 5])),
        )
        .expect("peer policy violation is accepted locally");
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        assert_eq!(right.channels.len(), 1);
        assert!(right.channel(StreamId(channels[1].value())).is_some());

        let replacement = channels[0];
        let mut replacement_config = config(DataReliability::Reliable, true);
        replacement_config.id = Some(replacement);
        left.open(at(now, start), replacement_config.clone())
            .expect("reuse only after the oversized stream's completed reset");
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        let received = right
            .channel(StreamId(replacement.value()))
            .unwrap()
            .received_messages;
        assert_eq!(right.max_inbound_sctp_message_bytes, MAX_DCEP_MESSAGE_BYTES);
        right.handle_message(Message::new(
            StreamId(replacement.value()),
            EMPTY_BINARY_PPID,
            vec![0; right.max_inbound_sctp_message_bytes + 1],
        ));
        assert!(matches!(
            right.channel(StreamId(replacement.value())).unwrap().state,
            ChannelState::Closing
        ));
        assert_eq!(
            right
                .channel(StreamId(replacement.value()))
                .unwrap()
                .received_messages,
            received
        );
        assert!(matches!(
            right.channel(StreamId(channels[1].value())).unwrap().state,
            ChannelState::Open
        ));
        assert_eq!(right.queued_inbound_payload_bytes, 0);
        assert!(!right.stopped);
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        left.open(at(now, start), replacement_config)
            .expect("wire-limit violation also waits for completed reset before reuse");
        drive(
            &mut left,
            &mut right,
            &mut now,
            start,
            &mut left_events,
            &mut right_events,
        );
        let mut payload = Vec::with_capacity(64);
        payload.extend_from_slice(&[7; 4]);
        let capacity = payload.capacity();
        assert_eq!(capacity, right.max_buffered_bytes);
        right.handle_message(Message::new(
            StreamId(channels[1].value()),
            BINARY_PPID,
            payload,
        ));
        assert_eq!(right.queued_inbound_payload_bytes, capacity);
        right.handle_message(Message::new(
            StreamId(channels[1].value()),
            EMPTY_TEXT_PPID,
            Vec::with_capacity(128),
        ));
        assert_eq!(right.queued_inbound_payload_bytes, capacity);
        let received = right
            .channel(StreamId(replacement.value()))
            .unwrap()
            .received_messages;
        right.handle_message(Message::new(
            StreamId(replacement.value()),
            BINARY_PPID,
            vec![9],
        ));
        assert_eq!(right.queued_inbound_payload_bytes, capacity);
        assert!(matches!(
            right.channel(StreamId(replacement.value())).unwrap().state,
            ChannelState::Closing
        ));
        assert_eq!(
            right
                .channel(StreamId(replacement.value()))
                .unwrap()
                .received_messages,
            received
        );
        assert!(matches!(
            right.channel(StreamId(channels[1].value())).unwrap().state,
            ChannelState::Open
        ));
        assert!(!right.stopped);
        let Some(Event::DataChannel(DataChannelEvent::Message {
            channel,
            message: DataMessage::Binary(held),
        })) = right.poll_event(at(now, start))
        else {
            panic!("healthy stream keeps its previously queued message");
        };
        assert_eq!(channel, channels[1]);
        assert_eq!(held.as_ref(), &[7; 4]);
        assert_eq!(right.queued_inbound_payload_bytes, 0);
        assert!(
            matches!(right.poll_event(at(now, start)), Some(Event::DataChannel(DataChannelEvent::Message { message: DataMessage::Text(bytes), .. })) if bytes.is_empty())
        );
        assert_eq!(right.queued_inbound_payload_bytes, 0);

        let deadline = now
            .checked_add(Duration::from_millis(20))
            .expect("deadline");
        right.begin_shutdown(at(now, start), deadline);
        let after = deadline
            .checked_add(Duration::from_millis(1))
            .expect("after");
        let _ = right.poll_event(at(after, start));
        assert!(right.stopped);
        assert!(!right.has_packet());
        let mut payload = Vec::with_capacity(64);
        payload.extend_from_slice(b"bye");
        left.handle_message(Message::new(
            StreamId(channels[1].value()),
            BINARY_PPID,
            payload,
        ));
        assert_eq!(left.queued_inbound_payload_bytes, 64);
        left.max_packet_bytes = 0;
        left.send(now, channels[1], DataMessage::Binary(Bytes::new()))
            .expect("native send commits before its packet cannot be retained");
        assert!(
            left.stopped,
            "unretainable emitted native packet fails the association closed"
        );
        assert_eq!(left.test_packet_bytes(), 0);
        assert!(
            left.events
                .iter()
                .any(|event| matches!(event.event, DataChannelEvent::Closed { .. })),
            "prepaid cutoff notifications survive packet exhaustion"
        );
        left.abort_association();
        assert_eq!(left.queued_inbound_payload_bytes, 64);
        left.abort();
        assert!(left.stopped);
        assert!(!left.has_packet());
        assert!(left.events.is_empty());
        assert_eq!(left.queued_inbound_payload_bytes, 0);
        let rejected = left
            .queue_packet(held_packet)
            .expect_err("late output after abort cannot resurrect storage");
        assert_eq!(rejected.as_ptr(), pointer);
        assert_eq!(left.test_packet_bytes(), 0);
        assert_eq!(left.outgoing.capacity(), 0);
        assert_eq!(held.as_ref(), &[7; 4]);
    }
}
