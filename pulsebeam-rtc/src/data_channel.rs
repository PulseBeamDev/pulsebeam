use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use dcsctp::api::{
    DcSctpSocket, Message, Options, PpId, SendOptions, SocketEvent, SocketTime,
    StreamId as SctpStreamId,
};

use crate::ChannelId;

const DCEP_PPID: u32 = 50;
const STRING_PPID: u32 = 51;
const BINARY_PPID: u32 = 53;
const STRING_EMPTY_PPID: u32 = 56;
const BINARY_EMPTY_PPID: u32 = 57;
const MAX_WORK_PER_TICK: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataChannelReliability {
    ordered: bool,
    max_retransmits: Option<u16>,
    max_lifetime: Option<Duration>,
}

impl DataChannelReliability {
    pub const fn reliable_ordered() -> Self {
        Self {
            ordered: true,
            max_retransmits: None,
            max_lifetime: None,
        }
    }

    pub const fn reliable_unordered() -> Self {
        Self {
            ordered: false,
            max_retransmits: None,
            max_lifetime: None,
        }
    }

    pub const fn max_retransmits(ordered: bool, max_retransmits: u16) -> Self {
        Self {
            ordered,
            max_retransmits: Some(max_retransmits),
            max_lifetime: None,
        }
    }

    pub const fn max_lifetime(ordered: bool, max_lifetime: Duration) -> Self {
        Self {
            ordered,
            max_retransmits: None,
            max_lifetime: Some(max_lifetime),
        }
    }

    pub const fn ordered(self) -> bool {
        self.ordered
    }

    pub const fn max_retransmits_value(self) -> Option<u16> {
        self.max_retransmits
    }

    pub const fn max_lifetime_value(self) -> Option<Duration> {
        self.max_lifetime
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataChannelOpen {
    id: ChannelId,
    label: String,
    protocol: String,
    reliability: DataChannelReliability,
}

impl DataChannelOpen {
    pub fn new(
        id: ChannelId,
        label: String,
        protocol: String,
        reliability: DataChannelReliability,
    ) -> Self {
        Self {
            id,
            label,
            protocol,
            reliability,
        }
    }

    pub const fn id(&self) -> ChannelId {
        self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    pub const fn reliability(&self) -> DataChannelReliability {
        self.reliability
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataChannelEvent {
    AssociationConnected,
    AssociationClosed,
    Open(DataChannelOpen),
    Message {
        id: ChannelId,
        binary: bool,
        payload: Vec<u8>,
    },
    Close(ChannelId),
    Error,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DataChannelError {
    #[error("channel {0:?} already exists")]
    DuplicateChannel(ChannelId),
    #[error("channel {0:?} does not exist")]
    UnknownChannel(ChannelId),
    #[error("channel label or protocol is too large")]
    OpenTooLarge,
    #[error("DCEP control message is malformed")]
    MalformedControl,
    #[error("DCEP channel type is unsupported")]
    UnsupportedReliability,
    #[error("SCTP send failed")]
    Send,
    #[error("SCTP egress capacity is exhausted")]
    EgressFull,
}

pub struct DataChannelAssociation {
    socket: Box<dyn DcSctpSocket>,
    epoch: Instant,
    channels: HashMap<ChannelId, DataChannelOpen>,
    events: VecDeque<DataChannelEvent>,
    egress: VecDeque<Vec<u8>>,
    event_capacity: usize,
    egress_capacity: usize,
}

impl DataChannelAssociation {
    pub fn new(
        name: &str,
        port: u16,
        now: Instant,
        event_capacity: usize,
        egress_capacity: usize,
    ) -> Self {
        let mut options = Options::default();
        options.local_port = port;
        options.remote_port = port;
        let socket = dcsctp::new_socket(name, &options);
        Self {
            socket,
            epoch: now,
            channels: HashMap::new(),
            events: VecDeque::with_capacity(event_capacity),
            egress: VecDeque::with_capacity(egress_capacity),
            event_capacity,
            egress_capacity,
        }
    }

    pub fn connect(&mut self, now: Instant) {
        self.advance(now);
        self.socket.connect();
        self.drain();
    }

    pub fn handle_input(&mut self, now: Instant, packet: &[u8]) {
        self.advance(now);
        self.socket.handle_input(packet);
        self.drain();
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        self.advance(now);
        self.drain();
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        let duration = self.socket.poll_timeout() - SocketTime::zero();
        if duration == Duration::MAX {
            None
        } else {
            self.epoch.checked_add(duration)
        }
    }

    pub fn open(&mut self, open: DataChannelOpen) -> Result<(), DataChannelError> {
        if self.channels.contains_key(&open.id) {
            return Err(DataChannelError::DuplicateChannel(open.id));
        }
        let payload = encode_open(&open)?;
        self.socket
            .send(
                Message::new(SctpStreamId(open.id.get()), PpId(DCEP_PPID), payload),
                &SendOptions::default(),
            )
            .map_err(|_| DataChannelError::Send)?;
        self.channels.insert(open.id, open.clone());
        self.push_event(DataChannelEvent::Open(open));
        self.drain();
        Ok(())
    }

    pub fn send(
        &mut self,
        id: ChannelId,
        binary: bool,
        payload: Vec<u8>,
    ) -> Result<(), DataChannelError> {
        let channel = self
            .channels
            .get(&id)
            .ok_or(DataChannelError::UnknownChannel(id))?;
        let ppid = match (binary, payload.is_empty()) {
            (false, false) => STRING_PPID,
            (true, false) => BINARY_PPID,
            (false, true) => STRING_EMPTY_PPID,
            (true, true) => BINARY_EMPTY_PPID,
        };
        self.socket
            .send(
                Message::new(SctpStreamId(id.get()), PpId(ppid), payload),
                &send_options(channel.reliability),
            )
            .map_err(|_| DataChannelError::Send)?;
        self.drain();
        Ok(())
    }

    pub fn close(&mut self, id: ChannelId) -> Result<(), DataChannelError> {
        if self.channels.remove(&id).is_none() {
            return Err(DataChannelError::UnknownChannel(id));
        }
        self.push_event(DataChannelEvent::Close(id));
        Ok(())
    }

    pub fn poll_event(&mut self) -> Option<DataChannelEvent> {
        self.events.pop_front()
    }

    pub fn poll_egress(&mut self) -> Option<Vec<u8>> {
        self.egress.pop_front()
    }

    pub fn egress_ready(&self) -> bool {
        self.egress.len() < self.egress_capacity
    }

    fn advance(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.epoch);
        self.socket.advance_time(SocketTime::zero() + elapsed);
    }

    fn drain(&mut self) {
        for _ in 0..MAX_WORK_PER_TICK {
            let Some(event) = self.socket.poll_event() else {
                break;
            };
            match event {
                SocketEvent::SendPacket(packet) => {
                    if self.egress_ready() {
                        self.egress.push_back(packet);
                    }
                }
                SocketEvent::OnConnected() => {
                    self.push_event(DataChannelEvent::AssociationConnected)
                }
                SocketEvent::OnClosed() => self.push_event(DataChannelEvent::AssociationClosed),
                SocketEvent::OnAborted(_, _) | SocketEvent::OnError(_, _) => {
                    self.push_event(DataChannelEvent::Error)
                }
                SocketEvent::OnIncomingStreamReset(streams) => {
                    for stream in streams.into_iter().take(MAX_WORK_PER_TICK) {
                        let id = ChannelId::new(stream.0);
                        if self.channels.remove(&id).is_some() {
                            self.push_event(DataChannelEvent::Close(id));
                        }
                    }
                }
                _ => {}
            }
        }
        for _ in 0..MAX_WORK_PER_TICK {
            let Some(message) = self.socket.get_next_message() else {
                break;
            };
            self.handle_message(message);
        }
    }

    fn handle_message(&mut self, message: Message) {
        let id = ChannelId::new(message.stream_id.0);
        match message.ppid.0 {
            DCEP_PPID => match decode_open(id, &message.payload) {
                Ok(Some(open)) => {
                    if self.channels.contains_key(&id) {
                        self.push_event(DataChannelEvent::Error);
                        return;
                    }
                    let ack = Message::new(message.stream_id, PpId(DCEP_PPID), vec![2]);
                    if self.socket.send(ack, &SendOptions::default()).is_err() {
                        self.push_event(DataChannelEvent::Error);
                        return;
                    }
                    self.channels.insert(id, open.clone());
                    self.push_event(DataChannelEvent::Open(open));
                }
                Ok(None) => {}
                Err(_) => self.push_event(DataChannelEvent::Error),
            },
            STRING_PPID => self.push_message(id, false, message.payload),
            BINARY_PPID => self.push_message(id, true, message.payload),
            STRING_EMPTY_PPID => self.push_message(id, false, Vec::new()),
            BINARY_EMPTY_PPID => self.push_message(id, true, Vec::new()),
            _ => self.push_event(DataChannelEvent::Error),
        }
    }

    fn push_message(&mut self, id: ChannelId, binary: bool, payload: Vec<u8>) {
        if self.channels.contains_key(&id) {
            self.push_event(DataChannelEvent::Message {
                id,
                binary,
                payload,
            });
        } else {
            self.push_event(DataChannelEvent::Error);
        }
    }

    fn push_event(&mut self, event: DataChannelEvent) {
        if self.events.len() < self.event_capacity {
            self.events.push_back(event);
        }
    }
}

fn send_options(reliability: DataChannelReliability) -> SendOptions {
    SendOptions {
        unordered: !reliability.ordered,
        lifetime: reliability.max_lifetime,
        max_retransmissions: reliability.max_retransmits,
        lifecycle_id: None,
    }
}

fn encode_open(open: &DataChannelOpen) -> Result<Vec<u8>, DataChannelError> {
    let label = open.label.as_bytes();
    let protocol = open.protocol.as_bytes();
    let label_length = u16::try_from(label.len()).map_err(|_| DataChannelError::OpenTooLarge)?;
    let protocol_length =
        u16::try_from(protocol.len()).map_err(|_| DataChannelError::OpenTooLarge)?;
    let (channel_type, reliability) = match (
        open.reliability.ordered,
        open.reliability.max_retransmits,
        open.reliability.max_lifetime,
    ) {
        (true, None, None) => (0x00, 0),
        (false, None, None) => (0x80, 0),
        (ordered, Some(value), None) => ((if ordered { 0x01 } else { 0x81 }), u32::from(value)),
        (ordered, None, Some(value)) => (
            if ordered { 0x02 } else { 0x82 },
            u32::try_from(value.as_millis()).map_err(|_| DataChannelError::OpenTooLarge)?,
        ),
        (_, Some(_), Some(_)) => return Err(DataChannelError::UnsupportedReliability),
    };
    let mut payload = Vec::with_capacity(
        12usize
            .saturating_add(label.len())
            .saturating_add(protocol.len()),
    );
    payload.extend_from_slice(&[3, channel_type]);
    payload.extend_from_slice(&0_u16.to_be_bytes());
    payload.extend_from_slice(&reliability.to_be_bytes());
    payload.extend_from_slice(&label_length.to_be_bytes());
    payload.extend_from_slice(&protocol_length.to_be_bytes());
    payload.extend_from_slice(label);
    payload.extend_from_slice(protocol);
    Ok(payload)
}

fn decode_open(id: ChannelId, payload: &[u8]) -> Result<Option<DataChannelOpen>, DataChannelError> {
    let Some(message_type) = payload.first() else {
        return Err(DataChannelError::MalformedControl);
    };
    if *message_type == 2 {
        return Ok(None);
    }
    if *message_type != 3 || payload.len() < 12 {
        return Err(DataChannelError::MalformedControl);
    }
    let channel_type = payload[1];
    let reliability_parameter =
        u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let label_length = usize::from(u16::from_be_bytes([payload[8], payload[9]]));
    let protocol_length = usize::from(u16::from_be_bytes([payload[10], payload[11]]));
    let label_end = 12usize
        .checked_add(label_length)
        .ok_or(DataChannelError::MalformedControl)?;
    let protocol_end = label_end
        .checked_add(protocol_length)
        .ok_or(DataChannelError::MalformedControl)?;
    let label = payload
        .get(12..label_end)
        .ok_or(DataChannelError::MalformedControl)?;
    let protocol = payload
        .get(label_end..protocol_end)
        .ok_or(DataChannelError::MalformedControl)?;
    if protocol_end != payload.len() {
        return Err(DataChannelError::MalformedControl);
    }
    let reliability = match channel_type {
        0x00 => DataChannelReliability::reliable_ordered(),
        0x80 => DataChannelReliability::reliable_unordered(),
        0x01 => DataChannelReliability::max_retransmits(
            true,
            u16::try_from(reliability_parameter).map_err(|_| DataChannelError::MalformedControl)?,
        ),
        0x81 => DataChannelReliability::max_retransmits(
            false,
            u16::try_from(reliability_parameter).map_err(|_| DataChannelError::MalformedControl)?,
        ),
        0x02 => DataChannelReliability::max_lifetime(
            true,
            Duration::from_millis(u64::from(reliability_parameter)),
        ),
        0x82 => DataChannelReliability::max_lifetime(
            false,
            Duration::from_millis(u64::from(reliability_parameter)),
        ),
        _ => return Err(DataChannelError::UnsupportedReliability),
    };
    let label =
        String::from_utf8(label.to_vec()).map_err(|_| DataChannelError::MalformedControl)?;
    let protocol =
        String::from_utf8(protocol.to_vec()).map_err(|_| DataChannelError::MalformedControl)?;
    Ok(Some(DataChannelOpen::new(id, label, protocol, reliability)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pump(left: &mut DataChannelAssociation, right: &mut DataChannelAssociation, now: Instant) {
        for _ in 0..16 {
            let mut moved = false;
            while let Some(packet) = left.poll_egress() {
                right.handle_input(now, &packet);
                moved = true;
            }
            while let Some(packet) = right.poll_egress() {
                left.handle_input(now, &packet);
                moved = true;
            }
            if !moved {
                break;
            }
        }
    }

    #[test]
    fn data_channel_preserves_reliability_in_dcep_open() {
        let open = DataChannelOpen::new(
            ChannelId::new(7),
            "updates".to_owned(),
            "json".to_owned(),
            DataChannelReliability::max_retransmits(false, 3),
        );

        let decoded = decode_open(open.id, &encode_open(&open).expect("encoded DCEP"))
            .expect("decoded DCEP")
            .expect("open message");

        assert_eq!(decoded, open);
    }

    #[test]
    fn data_channel_rejects_malformed_control() {
        let error = decode_open(ChannelId::new(7), &[3, 0]).expect_err("truncated control");

        assert_eq!(error, DataChannelError::MalformedControl);
    }

    #[test]
    fn data_channel_rejects_duplicate_channel_identity() {
        let now = Instant::now();
        let mut association = DataChannelAssociation::new("test", 5000, now, 4, 4);
        let open = DataChannelOpen::new(
            ChannelId::new(7),
            "updates".to_owned(),
            String::new(),
            DataChannelReliability::reliable_ordered(),
        );

        association.open(open.clone()).expect("first open");
        let error = association.open(open).expect_err("duplicate open");

        assert_eq!(error, DataChannelError::DuplicateChannel(ChannelId::new(7)));
    }

    #[test]
    fn data_channel_delivers_ordered_and_unordered_messages() {
        let now = Instant::now();
        let mut left = DataChannelAssociation::new("left", 5000, now, 16, 16);
        let mut right = DataChannelAssociation::new("right", 5000, now, 16, 16);
        left.connect(now);
        pump(&mut left, &mut right, now);
        let ordered = DataChannelOpen::new(
            ChannelId::new(7),
            "ordered".to_owned(),
            String::new(),
            DataChannelReliability::reliable_ordered(),
        );
        let unordered = DataChannelOpen::new(
            ChannelId::new(9),
            "unordered".to_owned(),
            String::new(),
            DataChannelReliability::reliable_unordered(),
        );
        left.open(ordered).expect("ordered open");
        left.open(unordered).expect("unordered open");
        pump(&mut left, &mut right, now);
        left.send(ChannelId::new(7), false, b"one".to_vec())
            .expect("ordered message");
        left.send(ChannelId::new(9), true, b"two".to_vec())
            .expect("unordered message");
        pump(&mut left, &mut right, now);

        let events: Vec<_> = std::iter::from_fn(|| right.poll_event()).collect();

        assert!(events.iter().any(|event| matches!(
            event,
            DataChannelEvent::Open(open)
                if open.id() == ChannelId::new(7)
                    && open.reliability() == DataChannelReliability::reliable_ordered()
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DataChannelEvent::Open(open)
                if open.id() == ChannelId::new(9)
                    && open.reliability() == DataChannelReliability::reliable_unordered()
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DataChannelEvent::Message { id, binary: false, payload }
                if *id == ChannelId::new(7) && payload == b"one"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DataChannelEvent::Message { id, binary: true, payload }
                if *id == ChannelId::new(9) && payload == b"two"
        )));
    }

    #[test]
    fn data_channel_exposes_retransmission_deadlines() {
        let now = Instant::now();
        let mut association = DataChannelAssociation::new("test", 5000, now, 4, 4);
        association.connect(now);

        assert!(association.next_deadline().is_some());
    }
}
