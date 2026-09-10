#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    reason = "SCTP wire fields use bounded arithmetic and the public DataChannel contract uses Arc strings and Bytes messages"
)]

use std::{collections::VecDeque, sync::Arc, time::Instant};

use bytes::Bytes;
use dcsctp::api::{
    DcSctpSocket, LifecycleId, Message, Options, PpId, ResetStreamsError, SendError, SendOptions,
    Socket, SocketEvent, SocketState, SocketTime, StreamId,
};

use crate::{
    CommandError, ConnectionLimits, DataChannelConfig, DataChannelEvent, DataChannelId,
    DataChannelPriority, DataChannelStats, DataMessage, DataReliability, Event, TimePoint,
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
const MAX_BUFFERED_MESSAGES: usize = 4_096;
const SCTP_MTU: usize = 1_191;
const MAX_DCEP_MESSAGE_BYTES: usize = 64 * 1024;

pub(crate) struct Association {
    socket: Socket,
    origin: Instant,
    local_stream_parity: u16,
    max_channels: usize,
    max_inbound_message_bytes: usize,
    max_outbound_message_bytes: usize,
    max_buffered_bytes: usize,
    max_events: usize,
    channels: Vec<Channel>,
    outgoing: VecDeque<Vec<u8>>,
    events: VecDeque<Event>,
    lifecycles: VecDeque<BufferedMessage>,
    next_lifecycle: u64,
    buffered_payload_bytes: usize,
    committed_transport_bytes: u64,
    shutdown_deadline: Option<Instant>,
    stopped: bool,
}

// SAFETY: dcsctp 0.1.14 uses Rc/RefCell only to share bookkeeping inside one
// Socket. Association uniquely owns that socket, never exposes a dcsctp handle,
// and all access requires &mut self, so moving the complete ownership graph to
// another thread cannot create concurrent access or a cross-thread drop race.
unsafe impl Send for Association {}

struct Channel {
    id: DataChannelId,
    stream: StreamId,
    config: DataChannelConfig,
    state: ChannelState,
    opened_emitted: bool,
    closed_emitted: bool,
    sent_messages: u64,
    received_messages: u64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ChannelState {
    Opening,
    Open,
    Closing,
}

struct BufferedMessage {
    lifecycle: u64,
    stream: StreamId,
    bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(dead_code, reason = "Plan 13 consumes private SCTP stats contributors")]
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
    ) -> Self {
        let max_outbound_message_bytes = if facts.unlimited_message_size {
            limits.max_buffered_data_bytes
        } else {
            facts
                .max_message_size
                .unwrap_or(64 * 1024)
                .min(limits.max_buffered_data_bytes)
        };
        let options = Options {
            local_port: facts.port,
            remote_port: facts.port,
            announced_maximum_incoming_streams: limits.max_data_channels.max(1),
            announced_maximum_outgoing_streams: limits.max_data_channels.max(1),
            mtu: SCTP_MTU,
            max_message_size: max_outbound_message_bytes.max(MAX_DCEP_MESSAGE_BYTES),
            max_receiver_window_buffer_size: limits.max_buffered_data_bytes.max(
                limits
                    .max_inbound_data_message_bytes
                    .saturating_add(SCTP_MTU),
            ),
            max_send_buffer_size: limits
                .max_buffered_data_bytes
                .saturating_add(MAX_DCEP_MESSAGE_BYTES),
            per_stream_send_queue_limit: limits
                .max_buffered_data_bytes
                .saturating_add(MAX_DCEP_MESSAGE_BYTES),
            total_buffered_amount_low_threshold: 0,
            enable_partial_reliability: true,
            enable_message_interleaving: true,
            ..Options::default()
        };
        Self {
            socket: Socket::new("pulsebeam-datachannels", &options),
            origin: accepted_at,
            local_stream_parity: u16::from(facts.local_dtls_role == DtlsRole::Passive),
            max_channels: usize::from(limits.max_data_channels),
            max_inbound_message_bytes: limits.max_inbound_data_message_bytes,
            max_outbound_message_bytes,
            max_buffered_bytes: limits.max_buffered_data_bytes,
            max_events: usize::from(limits.max_data_channels)
                .saturating_mul(2)
                .saturating_add(MAX_WORK_PER_CALL),
            channels: Vec::new(),
            outgoing: VecDeque::new(),
            events: VecDeque::new(),
            lifecycles: VecDeque::new(),
            next_lifecycle: 1,
            buffered_payload_bytes: 0,
            committed_transport_bytes: 0,
            shutdown_deadline: None,
            stopped: false,
        }
    }

    pub(crate) fn connect(&mut self, at: TimePoint) {
        if self.stopped || self.socket.state() != SocketState::Closed {
            return;
        }
        self.advance_time(at.monotonic);
        self.socket.connect();
        self.drain_socket();
    }

    pub(crate) fn open(
        &mut self,
        at: TimePoint,
        config: DataChannelConfig,
    ) -> Result<(), CommandError> {
        if self.stopped || self.socket.state() != SocketState::Connected {
            return Err(CommandError::InvalidState);
        }
        if self.channels.len() >= self.max_channels {
            return Err(CommandError::WouldBlock);
        }
        let stream = match config.id {
            Some(id) => StreamId(id.value()),
            None => self.allocate_stream().ok_or(CommandError::WouldBlock)?,
        };
        if self
            .channels
            .iter()
            .any(|channel| channel.stream == stream || config.id.is_some_and(|id| channel.id == id))
        {
            return Err(CommandError::InvalidState);
        }
        if config.label.len() > usize::from(u16::MAX)
            || config.protocol.len() > usize::from(u16::MAX)
            || 12_usize
                .saturating_add(config.label.len())
                .saturating_add(config.protocol.len())
                > MAX_DCEP_MESSAGE_BYTES
            || matches!(
                config.reliability,
                DataReliability::MaxLifetime(lifetime)
                    if lifetime.as_millis() > u128::from(u32::MAX)
            )
        {
            return Err(CommandError::InvalidState);
        }
        let id = config
            .id
            .unwrap_or_else(|| DataChannelId::from_value(stream.0));
        let negotiated = config.negotiated;
        self.socket
            .set_stream_priority(stream, config.priority.weight());
        self.channels.push(Channel {
            id,
            stream,
            config,
            state: if negotiated {
                ChannelState::Open
            } else {
                ChannelState::Opening
            },
            opened_emitted: false,
            closed_emitted: false,
            sent_messages: 0,
            received_messages: 0,
        });
        self.advance_time(at.monotonic);
        if negotiated {
            self.emit_opened(stream);
            return Ok(());
        }
        let Some(channel) = self.channel(stream) else {
            return Err(CommandError::InvalidState);
        };
        let payload = encode_dcep_open(&channel.config);
        if let Err(error) = self.socket.send(
            Message::new(stream, DCEP_PPID, payload),
            &SendOptions::default(),
        ) {
            self.channels.retain(|channel| channel.stream != stream);
            return Err(map_send_error(error));
        }
        self.drain_socket();
        Ok(())
    }

    pub(crate) fn send(
        &mut self,
        at: TimePoint,
        channel: DataChannelId,
        message: DataMessage,
    ) -> Result<(), CommandError> {
        let Some(entry) = self.channels.iter().find(|entry| entry.id == channel) else {
            return Err(CommandError::UnknownDataChannel(channel));
        };
        if self.stopped || entry.state != ChannelState::Open {
            return Err(CommandError::InvalidState);
        }
        let stream = entry.stream;
        let config = entry.config.clone();
        let payload_bytes = match &message {
            DataMessage::Text(bytes) | DataMessage::Binary(bytes) => bytes.len(),
        };
        if payload_bytes > self.max_outbound_message_bytes {
            return Err(CommandError::MessageTooLarge);
        }
        if self
            .buffered_payload_bytes
            .checked_add(payload_bytes)
            .is_none_or(|total| total > self.max_buffered_bytes)
        {
            return Err(CommandError::WouldBlock);
        }
        if self.lifecycles.len() >= MAX_BUFFERED_MESSAGES {
            return Err(CommandError::WouldBlock);
        }
        let (ppid, payload) = match message {
            DataMessage::Text(bytes) if bytes.is_empty() => (EMPTY_TEXT_PPID, vec![0]),
            DataMessage::Binary(bytes) if bytes.is_empty() => (EMPTY_BINARY_PPID, vec![0]),
            DataMessage::Text(bytes) => (TEXT_PPID, bytes.to_vec()),
            DataMessage::Binary(bytes) => (BINARY_PPID, bytes.to_vec()),
        };
        let lifecycle = self.next_lifecycle;
        self.next_lifecycle = self.next_lifecycle.checked_add(1).unwrap_or(1);
        let lifecycle_id = LifecycleId::new(lifecycle).ok_or(CommandError::WouldBlock)?;
        let options = send_options(&config, lifecycle_id);
        self.advance_time(at.monotonic);
        self.socket
            .send(Message::new(stream, ppid, payload), &options)
            .map_err(map_send_error)?;
        self.buffered_payload_bytes += payload_bytes;
        self.lifecycles.push_back(BufferedMessage {
            lifecycle,
            stream,
            bytes: payload_bytes,
        });
        if let Some(channel) = self.channel_mut(stream) {
            channel.sent_messages = channel.sent_messages.saturating_add(1);
        }
        self.drain_socket();
        Ok(())
    }

    pub(crate) fn close_channel(
        &mut self,
        at: TimePoint,
        id: DataChannelId,
    ) -> Result<(), CommandError> {
        let Some(index) = self.channels.iter().position(|channel| channel.id == id) else {
            return Err(CommandError::UnknownDataChannel(id));
        };
        let Some(entry) = self.channels.get_mut(index) else {
            return Err(CommandError::UnknownDataChannel(id));
        };
        if entry.state == ChannelState::Closing {
            return Ok(());
        }
        let stream = entry.stream;
        entry.state = ChannelState::Closing;
        self.advance_time(at.monotonic);
        match self.socket.reset_streams(&[stream]) {
            Ok(()) => {}
            Err(ResetStreamsError::NotConnected | ResetStreamsError::NotSupported) => {
                self.emit_closed(stream);
                self.channels.remove(index);
            }
        }
        self.drain_socket();
        Ok(())
    }

    pub(crate) fn handle_input(&mut self, at: TimePoint, packet: &[u8]) {
        if self.stopped {
            return;
        }
        self.advance_time(at.monotonic);
        self.socket.handle_input(packet);
        self.drain_socket();
        self.drain_messages();
    }

    pub(crate) fn poll_event(&mut self, at: TimePoint) -> Option<Event> {
        if self.stopped {
            return self.events.pop_front();
        }
        self.advance_time(at.monotonic);
        self.drain_socket();
        self.drain_messages();
        self.enforce_shutdown_deadline(at.monotonic);
        self.events.pop_front()
    }

    pub(crate) fn has_packet(&self) -> bool {
        !self.stopped && !self.outgoing.is_empty()
    }

    pub(crate) fn poll_packet(&mut self) -> Option<Vec<u8>> {
        self.outgoing.pop_front()
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

    #[allow(dead_code, reason = "Plan 13 consumes graceful SCTP shutdown")]
    pub(crate) fn begin_shutdown(&mut self, at: TimePoint, deadline: Instant) {
        if self.stopped {
            return;
        }
        self.advance_time(at.monotonic);
        self.shutdown_deadline = Some(deadline);
        self.socket.shutdown();
        self.drain_socket();
        self.enforce_shutdown_deadline(at.monotonic);
    }

    #[allow(dead_code, reason = "Plan 13 consumes immediate SCTP abort")]
    pub(crate) fn abort(&mut self) {
        if self.stopped {
            return;
        }
        self.socket.close();
        self.outgoing.clear();
        self.events.clear();
        self.channels.clear();
        self.lifecycles.clear();
        self.buffered_payload_bytes = 0;
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

    #[allow(dead_code, reason = "Plan 13 consumes private SCTP stats contributors")]
    pub(crate) fn stats(&self) -> AssociationStats {
        AssociationStats {
            buffered_payload_bytes: self.buffered_payload_bytes,
            committed_transport_bytes: self.committed_transport_bytes,
            channels: self.channels.len(),
        }
    }

    pub(crate) fn channel_stats(&self) -> Vec<DataChannelStats> {
        self.channels
            .iter()
            .map(|channel| DataChannelStats {
                channel: channel.id,
                priority: channel.config.priority,
                reliability: channel.config.reliability,
                buffered_amount: self
                    .lifecycles
                    .iter()
                    .filter(|message| message.stream == channel.stream)
                    .map(|message| message.bytes)
                    .fold(0_usize, usize::saturating_add),
                sent_messages: channel.sent_messages,
                received_messages: channel.received_messages,
            })
            .collect()
    }

    fn allocate_stream(&self) -> Option<StreamId> {
        (self.local_stream_parity..=u16::MAX)
            .step_by(2)
            .map(StreamId)
            .find(|stream| {
                !self
                    .channels
                    .iter()
                    .any(|channel| channel.stream == *stream)
            })
    }

    fn advance_time(&mut self, now: Instant) {
        self.socket
            .advance_time(SocketTime::zero() + now.saturating_duration_since(self.origin));
    }

    fn drain_socket(&mut self) {
        for _ in 0..MAX_WORK_PER_CALL {
            let Some(event) = self.socket.poll_event() else {
                break;
            };
            match event {
                SocketEvent::SendPacket(packet) => {
                    if self.outgoing.len() >= MAX_WORK_PER_CALL {
                        self.abort_association();
                        break;
                    }
                    self.outgoing.push_back(packet);
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
                | SocketEvent::OnStreamsResetFailed(_)
                | SocketEvent::OnLifecycleMessageFullySent(_)
                | SocketEvent::OnLifecycleMessageMaybeExpired(_)
                | SocketEvent::OnLifecycleMessageExpired(_)
                | SocketEvent::OnLifecycleMessageDelivered(_) => {}
                SocketEvent::OnStreamsResetPerformed(streams) => {
                    for stream in streams {
                        self.emit_closed(stream);
                        self.channels.retain(|channel| channel.stream != stream);
                    }
                }
                SocketEvent::OnIncomingStreamReset(streams) => {
                    let streams = if streams.is_empty() {
                        self.channels.iter().map(|channel| channel.stream).collect()
                    } else {
                        streams
                    };
                    for stream in streams {
                        self.emit_closed(stream);
                        if let Some(channel) = self.channel_mut(stream) {
                            channel.state = ChannelState::Closing;
                        }
                        let _ = self.socket.reset_streams(&[stream]);
                    }
                }
                SocketEvent::OnLifecycleEnd(id) => {
                    if let Some(index) = self
                        .lifecycles
                        .iter()
                        .position(|message| message.lifecycle == id.value())
                        && let Some(message) = self.lifecycles.remove(index)
                    {
                        self.buffered_payload_bytes =
                            self.buffered_payload_bytes.saturating_sub(message.bytes);
                    }
                }
            }
        }
    }

    fn drain_messages(&mut self) {
        for _ in 0..MAX_WORK_PER_CALL {
            let Some(message) = self.socket.get_next_message() else {
                break;
            };
            self.handle_message(message);
        }
    }

    fn handle_message(&mut self, message: Message) {
        let stream = message.stream_id;
        match message.ppid {
            DCEP_PPID => {
                if message.payload.len() > MAX_DCEP_MESSAGE_BYTES {
                    self.close_stream(stream);
                } else {
                    self.handle_dcep(stream, &message.payload);
                }
            }
            TEXT_PPID | BINARY_PPID | EMPTY_TEXT_PPID | EMPTY_BINARY_PPID => {
                let Some(channel) = self.channel(stream) else {
                    self.close_stream(stream);
                    return;
                };
                if channel.state != ChannelState::Open {
                    return;
                }
                let empty = message.ppid == EMPTY_TEXT_PPID || message.ppid == EMPTY_BINARY_PPID;
                let semantic_len = if empty { 0 } else { message.payload.len() };
                if semantic_len > self.max_inbound_message_bytes {
                    self.close_stream(stream);
                    return;
                }
                let id = channel.id;
                if let Some(channel) = self.channel_mut(stream) {
                    channel.received_messages = channel.received_messages.saturating_add(1);
                }
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
                self.push_event(Event::DataChannel(DataChannelEvent::Message {
                    channel: id,
                    message,
                }));
            }
            _ => self.close_stream(stream),
        }
    }

    fn handle_dcep(&mut self, stream: StreamId, payload: &[u8]) {
        match payload.first().copied() {
            Some(DCEP_ACK) if payload == [DCEP_ACK] => {
                if let Some(channel) = self.channel_mut(stream)
                    && channel.state == ChannelState::Opening
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
                    || self.channels.len() >= self.max_channels
                {
                    self.close_stream(stream);
                    return;
                }
                let Some(config) = decode_dcep_open(payload) else {
                    self.close_stream(stream);
                    return;
                };
                let id = DataChannelId::from_value(stream.0);
                self.socket
                    .set_stream_priority(stream, config.priority.weight());
                self.channels.push(Channel {
                    id,
                    stream,
                    config,
                    state: ChannelState::Open,
                    opened_emitted: false,
                    closed_emitted: false,
                    sent_messages: 0,
                    received_messages: 0,
                });
                let _ = self.socket.send(
                    Message::new(stream, DCEP_PPID, vec![DCEP_ACK]),
                    &SendOptions::default(),
                );
                self.emit_opened(stream);
                self.drain_socket();
            }
            _ => self.close_stream(stream),
        }
    }

    fn close_stream(&mut self, stream: StreamId) {
        self.emit_closed(stream);
        if let Some(channel) = self.channel_mut(stream) {
            channel.state = ChannelState::Closing;
        }
        let _ = self.socket.reset_streams(&[stream]);
        self.drain_socket();
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
        self.push_event(Event::DataChannel(DataChannelEvent::Opened { channel: id }));
    }

    fn emit_closed(&mut self, stream: StreamId) {
        let Some(channel) = self.channel_mut(stream) else {
            return;
        };
        if channel.closed_emitted {
            return;
        }
        channel.closed_emitted = true;
        let id = channel.id;
        self.push_event(Event::DataChannel(DataChannelEvent::Closed { channel: id }));
    }

    fn close_all_channels(&mut self) {
        let streams: Vec<_> = self.channels.iter().map(|channel| channel.stream).collect();
        for stream in streams {
            self.emit_closed(stream);
        }
    }

    fn abort_association(&mut self) {
        self.socket.close();
        self.outgoing.clear();
        self.close_all_channels();
        self.stopped = true;
    }

    fn enforce_shutdown_deadline(&mut self, now: Instant) {
        if self
            .shutdown_deadline
            .is_some_and(|deadline| now >= deadline)
            && self.socket.state() != SocketState::Closed
        {
            self.abort_association();
        }
    }

    fn channel(&self, stream: StreamId) -> Option<&Channel> {
        self.channels
            .iter()
            .find(|channel| channel.stream == stream)
    }

    fn channel_mut(&mut self, stream: StreamId) -> Option<&mut Channel> {
        self.channels
            .iter_mut()
            .find(|channel| channel.stream == stream)
    }

    fn push_event(&mut self, event: Event) {
        if self.events.len() < self.max_events {
            self.events.push_back(event);
        } else {
            self.socket.close();
            self.outgoing.clear();
            self.channels.clear();
            self.lifecycles.clear();
            self.buffered_payload_bytes = 0;
            self.stopped = true;
        }
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

fn map_send_error(error: SendError) -> CommandError {
    match error {
        SendError::MessageTooLarge { .. } => CommandError::MessageTooLarge,
        SendError::ResourceExhaustion => CommandError::WouldBlock,
        SendError::EmptyPayload | SendError::ShuttingDown => CommandError::InvalidState,
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
    let mut output = Vec::with_capacity(12 + label.len() + protocol.len());
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
    let header: &[u8; 12] = payload.get(..12)?.try_into().ok()?;
    if header[0] != DCEP_OPEN || header[1] & 0x7c != 0 {
        return None;
    }
    let label_len = usize::from(u16::from_be_bytes([header[8], header[9]]));
    let protocol_len = usize::from(u16::from_be_bytes([header[10], header[11]]));
    let expected = 12_usize.checked_add(label_len)?.checked_add(protocol_len)?;
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
    let label_end = 12 + label_len;
    let label = std::str::from_utf8(payload.get(12..label_end)?).ok()?;
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
    use crate::GlobalMediaTime;
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
    }

    #[test]
    fn data_channel_malformed_dcep_is_rejected() {
        let mut payload = encode_dcep_open(&config(DataReliability::Reliable, true));
        payload[1] = 0x7f;
        assert!(decode_dcep_open(&payload).is_none());
        payload = encode_dcep_open(&config(DataReliability::Reliable, true));
        payload.pop();
        assert!(decode_dcep_open(&payload).is_none());
    }

    fn facts(role: DtlsRole) -> SctpSessionFacts {
        SctpSessionFacts {
            port: 5000,
            max_message_size: Some(65_536),
            unlimited_message_size: false,
            local_dtls_role: role,
        }
    }

    fn at(now: Instant, start: Instant) -> TimePoint {
        TimePoint {
            monotonic: now,
            global: GlobalMediaTime::from_micros(
                u64::try_from(now.duration_since(start).as_micros()).unwrap_or(u64::MAX),
            ),
        }
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
        let start = Instant::now();
        let mut now = start;
        let mut left = Association::new(facts(DtlsRole::Active), left_limits, start);
        let mut right = Association::new(facts(DtlsRole::Passive), right_limits, start);
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
            Event::DataChannel(DataChannelEvent::Opened { channel }) => Some(*channel),
            _ => None,
        })
    }

    #[test]
    fn data_channel_associations_fragment_reassemble_deduplicate_dcep_and_reuse_streams() {
        let limits = ConnectionLimits {
            max_data_channels: 1,
            ..ConnectionLimits::default()
        };
        let (mut left, mut right, mut now, start) = connected_pair(limits, limits);
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
        assert_eq!(
            left.open(at(now, start), config(DataReliability::Reliable, true)),
            Err(CommandError::WouldBlock)
        );

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
            DataMessage::Binary(Bytes::from(vec![9; 8_000])),
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
            }) if bytes.len() == 8_000
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
        assert_eq!(left.channels[0].stream, stream);
    }

    #[test]
    fn data_channel_negotiated_stream_preserves_empty_message_boundaries() {
        let limits = ConnectionLimits {
            max_data_channels: 1,
            ..ConnectionLimits::default()
        };
        let (mut left, mut right, mut now, start) = connected_pair(limits, limits);
        let mut negotiated = config(DataReliability::Reliable, true);
        negotiated.id = Some(DataChannelId::from_value(7));
        negotiated.negotiated = true;
        left.open(at(now, start), negotiated.clone())
            .expect("left negotiated channel");
        right
            .open(at(now, start), negotiated)
            .expect("right negotiated channel");
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
        left.send(
            at(now, start),
            DataChannelId::from_value(7),
            DataMessage::Text(Bytes::new()),
        )
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
            }) if *channel == DataChannelId::from_value(7) && bytes.is_empty()
        )));
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
        left.send(
            at(now, start),
            channel,
            DataMessage::Binary(Bytes::from_static(&[1; 48])),
        )
        .expect("first message");
        assert_eq!(
            left.send(
                at(now, start),
                channel,
                DataMessage::Binary(Bytes::from_static(&[2; 24])),
            ),
            Err(CommandError::WouldBlock)
        );
        assert_eq!(left.stats().buffered_payload_bytes, 48);
    }

    #[test]
    fn data_channel_oversized_input_isolated_and_shutdown_bounded() {
        let left_limits = ConnectionLimits {
            max_buffered_data_bytes: 64,
            max_data_channels: 2,
            ..ConnectionLimits::default()
        };
        let right_limits = ConnectionLimits {
            max_inbound_data_message_bytes: 4,
            max_buffered_data_bytes: 64,
            max_data_channels: 2,
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
                Event::DataChannel(DataChannelEvent::Opened { channel }) => Some(*channel),
                _ => None,
            })
            .collect();
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
        left.abort();
        assert!(left.stopped);
        assert!(!left.has_packet());
    }
}
