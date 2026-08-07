use crate::RtpPacket;
use crate::agent::mailbox;
use crate::manager::VideoSubscription;
use pulsebeam_proto::signaling::Track;
use str0m::channel::ChannelId;
use str0m::media::{Mid, Rid};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PublicationLease {
    pub(crate) mid: Mid,
    pub(crate) generation: u64,
}

pub(crate) enum OutgoingCommand {
    SendData(SendData),
    SendMedia(SendMedia),
    SetPlayoutDelay(Option<(u32, u32)>),
    Publish {
        kind: str0m::media::MediaKind,
        response: tokio::sync::oneshot::Sender<Result<super::LocalTrack, super::AgentError>>,
    },
    Unpublish {
        lease: PublicationLease,
        response: Option<tokio::sync::oneshot::Sender<Result<(), super::AgentError>>>,
    },
    SubscribeMedia {
        subscription: VideoSubscription,
        response: tokio::sync::oneshot::Sender<Result<RemoteTrack, super::AgentError>>,
    },
    Shutdown(tokio::sync::oneshot::Sender<()>),
    DeclareOrderedPublisher {
        topic: String,
        response: tokio::sync::oneshot::Sender<Result<OrderedTopicPublisher, super::AgentError>>,
    },
    DeclareOrderedSubscriber {
        topic: String,
        response: tokio::sync::oneshot::Sender<Result<OrderedTopicSubscriber, super::AgentError>>,
    },
    DeclareLatestPublisher {
        topic: String,
        response: tokio::sync::oneshot::Sender<Result<DataPublisher, super::AgentError>>,
    },
    DeclareLatestSubscriber {
        topic: String,
        publisher_id: Option<String>,
        response: tokio::sync::oneshot::Sender<Result<DataSubscriber, super::AgentError>>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct SendData {
    pub(crate) channel_id: ChannelId,
    pub(crate) payload: Vec<u8>,
}

/// Payload for sending one RTP packet.
#[derive(Clone, Debug)]
pub(crate) struct SendMedia {
    pub(crate) lease: PublicationLease,
    pub(crate) rid: Option<Rid>,
    pub(crate) packet: RtpPacket,
}

#[derive(Clone)]
pub struct DataPublisher {
    channel_id: ChannelId,
    topic: String,
    pub(crate) tx: mailbox::Sender<OutgoingCommand>,
}

impl DataPublisher {
    pub(crate) fn new(
        channel_id: ChannelId,
        topic: String,
        tx: mailbox::Sender<OutgoingCommand>,
    ) -> Self {
        Self {
            channel_id,
            topic,
            tx,
        }
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub async fn send(&self, payload: Vec<u8>) -> Result<(), mailbox::SendError<Vec<u8>>> {
        let command = OutgoingCommand::SendData(SendData {
            channel_id: self.channel_id,
            payload,
        });

        self.tx.send(command).await.map_err(|err| match err.0 {
            OutgoingCommand::SendData(data) => mailbox::SendError(data.payload),
            _ => unreachable!(),
        })
    }

    pub fn try_send(&self, payload: Vec<u8>) -> Result<(), mailbox::TrySendError<Vec<u8>>> {
        let command = OutgoingCommand::SendData(SendData {
            channel_id: self.channel_id,
            payload,
        });

        self.tx.try_send(command).map_err(|err| match err {
            mailbox::TrySendError::Full(OutgoingCommand::SendData(data)) => {
                mailbox::TrySendError::Full(data.payload)
            }
            mailbox::TrySendError::Disconnected(OutgoingCommand::SendData(data)) => {
                mailbox::TrySendError::Disconnected(data.payload)
            }
            _ => unreachable!(),
        })
    }
}

#[derive(Clone)]
pub struct DataSubscriber {
    topic: String,
    scope: Option<String>,
    pub(crate) rx: mailbox::Receiver<Vec<u8>>,
}

impl DataSubscriber {
    pub(crate) fn new(
        topic: String,
        scope: Option<String>,
        rx: mailbox::Receiver<Vec<u8>>,
    ) -> Self {
        Self { topic, scope, rx }
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub fn publisher_id(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>, mailbox::RecvError> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Result<Vec<u8>, mailbox::TryRecvError> {
        self.rx.try_recv()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderedTopicMessage {
    pub publisher_id: String,
    pub stream_id: u64,
    pub seq: u64,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OrderedTopicDelivery {
    Message(OrderedTopicMessage),
    ResyncRequired {
        publisher_id: String,
        new_stream_id: u64,
    },
}

#[derive(Clone)]
pub struct OrderedTopicPublisher {
    pub(crate) topic: String,
    pub(crate) channel_id: ChannelId,
    pub(crate) tx: mailbox::Sender<OutgoingCommand>,
}

impl OrderedTopicPublisher {
    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub async fn send(&self, payload: Vec<u8>) -> Result<(), mailbox::SendError<Vec<u8>>> {
        let command = OutgoingCommand::SendData(SendData {
            channel_id: self.channel_id,
            payload,
        });
        self.tx.send(command).await.map_err(|err| match err.0 {
            OutgoingCommand::SendData(data) => mailbox::SendError(data.payload),
            _ => unreachable!(),
        })
    }

    pub fn try_send(&self, payload: Vec<u8>) -> Result<(), mailbox::TrySendError<Vec<u8>>> {
        let command = OutgoingCommand::SendData(SendData {
            channel_id: self.channel_id,
            payload,
        });
        self.tx.try_send(command).map_err(|error| match error {
            mailbox::TrySendError::Full(OutgoingCommand::SendData(data)) => {
                mailbox::TrySendError::Full(data.payload)
            }
            mailbox::TrySendError::Disconnected(OutgoingCommand::SendData(data)) => {
                mailbox::TrySendError::Disconnected(data.payload)
            }
            _ => unreachable!(),
        })
    }
}

pub struct OrderedTopicSubscriber {
    pub(crate) topic: String,
    pub(crate) rx: mailbox::Receiver<OrderedTopicDelivery>,
}

impl OrderedTopicSubscriber {
    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub async fn recv(&mut self) -> Result<OrderedTopicDelivery, mailbox::RecvError> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Result<OrderedTopicDelivery, mailbox::TryRecvError> {
        self.rx.try_recv()
    }
}

#[derive(Clone)]
pub struct LocalEncoding {
    pub(crate) mid: Mid,
    pub(crate) rid: Option<Rid>,
    pub(crate) lease: PublicationLease,
    pub(crate) keyframe_rx: crate::media::KeyframeReceiver,
    pub(crate) tx: mailbox::Sender<OutgoingCommand>,
}

impl LocalEncoding {
    pub fn rid(&self) -> Option<&str> {
        self.rid.as_deref()
    }

    pub async fn send(&self, packet: RtpPacket) -> Result<(), mailbox::SendError<RtpPacket>> {
        self.tx
            .send(OutgoingCommand::SendMedia(SendMedia {
                lease: self.lease,
                rid: self.rid,
                packet,
            }))
            .await
            .map_err(|error| match error.0 {
                OutgoingCommand::SendMedia(media) => mailbox::SendError(media.packet),
                _ => unreachable!(),
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Publication {
    id: String,
    publisher_id: String,
    kind: Option<str0m::media::MediaKind>,
}

impl Publication {
    pub(crate) fn from_signaling(track: Track) -> Self {
        debug_assert!(!track.id.is_empty());
        debug_assert!(!track.participant_id.is_empty());
        let kind = match track.kind {
            1 => Some(str0m::media::MediaKind::Video),
            2 => Some(str0m::media::MediaKind::Audio),
            _ => None,
        };
        Self {
            id: track.id,
            publisher_id: track.participant_id,
            kind,
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn publisher_id(&self) -> &str {
        &self.publisher_id
    }

    pub(crate) fn kind(&self) -> Option<str0m::media::MediaKind> {
        self.kind
    }
}

pub struct RemoteTrack {
    publication: Publication,
    pub(crate) rx: mailbox::Receiver<RtpPacket>,
}

impl RemoteTrack {
    pub(crate) fn new(_mid: Mid, track: Track, rx: mailbox::Receiver<RtpPacket>) -> Self {
        Self {
            publication: Publication::from_signaling(track),
            rx,
        }
    }

    pub fn publisher_id(&self) -> &str {
        self.publication.publisher_id()
    }

    /// Receive the next RTP packet for this track. Frame reassembly, jitter
    /// buffering, and decryption are higher-level concerns — see
    /// [`crate::pipeline::FrameReceiver`].
    pub async fn recv(&mut self) -> Result<RtpPacket, mailbox::RecvError> {
        self.rx.recv().await
    }
}
