pub mod pulsebeam {
    pub mod v1 {
        use anyhow::Context;
        tonic::include_proto!("pulsebeam.v1");

        impl Ord for PeerInfo {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                match self.group_id.cmp(&other.group_id) {
                    std::cmp::Ordering::Equal => match self.peer_id.cmp(&other.peer_id) {
                        std::cmp::Ordering::Equal => self.conn_id.cmp(&other.conn_id),
                        ordering => ordering,
                    },
                    ordering => ordering,
                }
            }
        }

        impl PartialOrd for PeerInfo {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl std::fmt::Display for PeerInfo {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}:{}:{}", self.group_id, self.peer_id, self.conn_id)
            }
        }

        pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("descriptor");

        #[derive(Debug)]
        pub struct ValidatedMessageHeader {
            pub src: PeerInfo,
            pub dst: PeerInfo,
            pub seqnum: u32,
            pub reliable: bool,
        }

        impl TryFrom<MessageHeader> for ValidatedMessageHeader {
            type Error = anyhow::Error;
            fn try_from(value: MessageHeader) -> Result<Self, Self::Error> {
                let src = value.src.context("src is required in message header")?;
                let dst = value.dst.context("dst is required in message header")?;
                Ok(Self {
                    src,
                    dst,
                    seqnum: value.seqnum,
                    reliable: value.reliable,
                })
            }
        }

        impl From<ValidatedMessageHeader> for MessageHeader {
            fn from(value: ValidatedMessageHeader) -> Self {
                Self {
                    src: Some(value.src),
                    dst: Some(value.dst),
                    seqnum: value.seqnum,
                    reliable: value.reliable,
                }
            }
        }

        #[derive(Debug)]
        pub struct ValidatedMessage {
            pub header: ValidatedMessageHeader,
            pub payload: MessagePayload,
        }

        impl ValidatedMessage {
            pub fn new_join(src: PeerInfo, dst: PeerInfo) -> Self {
                Self {
                    header: ValidatedMessageHeader {
                        src,
                        dst,
                        seqnum: 0,
                        reliable: false,
                    },
                    payload: MessagePayload {
                        payload_type: Some(message_payload::PayloadType::Join(Join {})),
                    },
                }
            }
        }

        impl TryFrom<Message> for ValidatedMessage {
            type Error = anyhow::Error;

            fn try_from(value: Message) -> Result<Self, Self::Error> {
                let header = value.header.context("message header is required")?;
                let header = ValidatedMessageHeader::try_from(header)?;
                let payload = value.payload.context("payload is required")?;
                Ok(Self { header, payload })
            }
        }

        impl From<ValidatedMessage> for Message {
            fn from(value: ValidatedMessage) -> Self {
                Self {
                    header: Some(value.header.into()),
                    payload: Some(value.payload),
                }
            }
        }
    }
}
pub use pulsebeam::v1::*;
