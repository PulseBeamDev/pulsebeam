use crate::MediaFrame;
use crate::agent::mailbox;
use crate::manager::Subscription;
use pulsebeam_proto::signaling::Track;
use str0m::channel::ChannelId;
use str0m::media::{Mid, Rid};

#[derive(Clone)]
pub(crate) enum OutgoingCommand {
    SendData(SendData),
    SendMedia(SendMedia),
    SetSubscription(Subscription),
}

#[derive(Clone, Debug)]
pub(crate) struct SendData {
    pub(crate) channel_id: ChannelId,
    pub(crate) payload: Vec<u8>,
}

/// Payload for sending media.
#[derive(Clone, Debug)]
pub(crate) struct SendMedia {
    pub(crate) mid: Mid,
    pub(crate) rid: Option<Rid>,
    pub(crate) frame: MediaFrame,
}

#[derive(Clone)]
pub struct DataPublisher {
    pub channel_id: ChannelId,
    pub topic: String,
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
    pub channel_id: ChannelId,
    pub topic: String,
    pub(crate) rx: mailbox::Receiver<Vec<u8>>,
}

impl DataSubscriber {
    pub(crate) fn new(
        channel_id: ChannelId,
        topic: String,
        rx: mailbox::Receiver<Vec<u8>>,
    ) -> Self {
        Self {
            channel_id,
            topic,
            rx,
        }
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>, mailbox::RecvError> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Result<Vec<u8>, mailbox::TryRecvError> {
        self.rx.try_recv()
    }
}

pub struct LocalTrack {
    pub kind: str0m::media::MediaKind,
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub keyframe_rx: crate::media::KeyframeReceiver,
    pub(crate) tx: mailbox::Sender<OutgoingCommand>,
}

impl LocalTrack {
    pub async fn send(&self, frame: MediaFrame) {
        let _ = self
            .tx
            .send(OutgoingCommand::SendMedia(SendMedia {
                mid: self.mid,
                rid: self.rid,
                frame,
            }))
            .await;
    }
}

pub struct RemoteTrack {
    pub mid: Mid,
    pub track: Track,
    pub(crate) rx: mailbox::Receiver<MediaFrame>,
}

impl RemoteTrack {
    pub async fn recv(&mut self) -> Result<MediaFrame, mailbox::RecvError> {
        self.rx.recv().await
    }
}
