pub mod pulsebeam {
    pub mod v1 {
        tonic::include_proto!("pulsebeam.v1");
    }
}

use moka::future::Cache;
use pulsebeam::v1::signaling_server::Signaling;
pub use pulsebeam::v1::signaling_server::SignalingServer;
use pulsebeam::v1::{
    IceServer, Message, PeerInfo, PrepareReq, PrepareResp, RecvResp, SendReq, SendResp,
};
use std::pin::Pin;
use tokio::time;
use tokio_stream::{Stream, StreamExt};

const SESSION_POLL_TIMEOUT: time::Duration = time::Duration::from_secs(1200);
const RESERVED_CONN_ID_DISCOVERY: u32 = 0;

type Mailbox = (flume::Sender<Message>, flume::Receiver<Message>);

#[derive(Clone)]
pub struct Server {
    mailboxes: Cache<PeerInfo, Mailbox, ahash::RandomState>,
}

pub struct ServerConfig {
    pub max_capacity: u64,
}

impl Server {
    pub fn new(cfg: ServerConfig) -> Self {
        Self {
            // | peerA | peerB | description |
            // | t+0   | t+SESSION_POLL_TIMEOUT | let peerA messages drop, peerB will initiate |
            // | t+0   | t+SESSION_POLL_TIMEOUT-SESSION_POLL_LATENCY_TOLERANCE | peerB receives messages then it'll refresh cache on the next poll |
            mailboxes: Cache::builder()
                .time_to_idle(SESSION_POLL_TIMEOUT)
                .max_capacity(cfg.max_capacity)
                .build_with_hasher(ahash::RandomState::default()),
        }
    }

    #[inline]
    async fn get(&self, info: &PeerInfo) -> Mailbox {
        self.mailboxes
            .get_with_by_ref(info, async { flume::bounded(0) })
            .await
    }
}

type RecvStream = Pin<Box<dyn Stream<Item = Result<RecvResp, tonic::Status>> + Send>>;

#[tonic::async_trait]
impl Signaling for Server {
    async fn prepare(
        &self,
        _req: tonic::Request<PrepareReq>,
    ) -> Result<tonic::Response<PrepareResp>, tonic::Status> {
        // WARNING: PLEASE READ THIS FIRST!
        // By default, OSS/self-hosting only provides a public STUN server.
        // You must provide your own TURN and STUN services.
        // TURN is required in some network condition.
        // Public TURN and STUN services are UNRELIABLE.
        Ok(tonic::Response::new(PrepareResp {
            ice_servers: vec![IceServer {
                urls: vec![String::from("stun:stun.l.google.com:19302")],
                username: None,
                credential: None,
            }],
        }))
    }

    async fn send(
        &self,
        req: tonic::Request<SendReq>,
    ) -> Result<tonic::Response<SendResp>, tonic::Status> {
        let msg = req
            .into_inner()
            .msg
            .ok_or(tonic::Status::invalid_argument("msg is required"))?;
        let hdr = msg
            .header
            .as_ref()
            .ok_or(tonic::Status::invalid_argument("header is required"))?;
        let dst = hdr
            .dst
            .as_ref()
            .ok_or(tonic::Status::invalid_argument("dst is required"))?;

        let (ch, _) = self.get(dst).await;
        ch.send_async(msg)
            .await
            .map_err(|err| tonic::Status::aborted(err.to_string()))?;

        Ok(tonic::Response::new(SendResp {}))
    }

    type RecvStream = RecvStream;
    async fn recv(
        &self,
        req: tonic::Request<pulsebeam::v1::RecvReq>,
    ) -> std::result::Result<tonic::Response<Self::RecvStream>, tonic::Status> {
        let src = req
            .into_inner()
            .src
            .ok_or(tonic::Status::invalid_argument("src is required"))?;

        if src.conn_id == RESERVED_CONN_ID_DISCOVERY {
            return Err(tonic::Status::invalid_argument(
                "conn_id must not use reserved conn ids",
            ));
        }

        let discovery_info = PeerInfo {
            conn_id: RESERVED_CONN_ID_DISCOVERY,
            ..src.clone()
        };
        let (_, discovery) = self.get(&discovery_info).await;
        let (_, payload) = self.get(&src).await;

        let discovery_stream = discovery
            .into_stream()
            .map(|msg| Ok(RecvResp { msg: Some(msg) }));
        let payload_stream = payload
            .into_stream()
            .map(|msg| Ok(RecvResp { msg: Some(msg) }));
        let merged_stream = discovery_stream.merge(payload_stream);

        Ok(tonic::Response::new(
            Box::pin(merged_stream) as Self::RecvStream
        ))
    }
}

#[cfg(test)]
mod test {
    use std::iter::zip;

    use super::*;
    use pulsebeam::v1::{MessageHeader, MessagePayload, RecvReq};
    use tokio::task::JoinSet;
    const SESSION_BATCH_MAX: usize = 128;

    fn dummy_msg(src: PeerInfo, dst: PeerInfo, seqnum: u32) -> Message {
        Message {
            header: Some(MessageHeader {
                src: Some(src),
                dst: Some(dst),
                seqnum,
                reliable: true,
            }),
            payload: Some(MessagePayload { payload_type: None }),
        }
    }

    fn assert_msgs(received: &[Message], sent: &[Message]) {
        assert_eq!(received.len(), sent.len());
        let mut received = received.to_vec();
        received.sort_by_key(|m| m.header.as_ref().unwrap().seqnum);
        let mut sent = sent.to_vec();
        sent.sort_by_key(|m| m.header.as_ref().unwrap().seqnum);
        let pairs = zip(received, sent);
        for (a, b) in pairs.into_iter() {
            assert_eq!(a, b);
        }
    }

    async fn assert_stream(received: tonic::Response<RecvStream>, sent: &[Message], take: usize) {
        let received_msgs: Vec<Message> = received
            .into_inner()
            .filter_map(|r| r.ok())
            .filter_map(|r| r.msg)
            .take(take)
            .collect()
            .await;
        assert_msgs(&received_msgs, sent)
    }

    fn setup() -> (Server, PeerInfo, PeerInfo) {
        let s = Server::new(ServerConfig {
            max_capacity: 10_000,
        });
        let peer1 = PeerInfo {
            group_id: String::from("default"),
            peer_id: String::from("peer1"),
            conn_id: 32,
        };
        let peer2 = PeerInfo {
            group_id: peer1.group_id.clone(),
            peer_id: String::from("peer2"),
            conn_id: 64,
        };
        (s, peer1, peer2)
    }

    #[tokio::test]
    async fn recv_normal() {
        let (s, peer1, peer2) = setup();
        let msgs = vec![dummy_msg(peer1.clone(), peer2.clone(), 0)];
        let (send, recv) = tokio::join!(
            s.send(tonic::Request::new(SendReq {
                msg: Some(msgs[0].clone()),
            })),
            s.recv(tonic::Request::new(RecvReq {
                src: Some(peer2.clone()),
            }))
        );

        send.unwrap();
        let resp = recv.unwrap();
        assert_stream(resp, &msgs, 1).await;
    }

    #[tokio::test]
    async fn recv_normal_many() {
        let (s, peer1, peer2) = setup();
        let msgs = vec![
            dummy_msg(peer1.clone(), peer2.clone(), 0),
            dummy_msg(peer1.clone(), peer2.clone(), 1),
        ];
        let (send1, send2, recv) = tokio::join!(
            s.send(tonic::Request::new(SendReq {
                msg: Some(msgs[0].clone()),
            })),
            s.send(tonic::Request::new(SendReq {
                msg: Some(msgs[1].clone()),
            })),
            s.recv(tonic::Request::new(RecvReq {
                src: Some(peer2.clone()),
            }))
        );

        send1.unwrap();
        send2.unwrap();
        let resp = recv.unwrap();
        assert_stream(resp, &msgs, 2).await;
    }

    #[tokio::test]
    async fn recv_normal_more_than_max() {
        let (s, peer1, peer2) = setup();
        let mut msgs = Vec::with_capacity(SESSION_BATCH_MAX + 1);
        let mut join_set = JoinSet::new();
        for i in 0..SESSION_BATCH_MAX + 1 {
            let msg = dummy_msg(peer1.clone(), peer2.clone(), i as u32);
            msgs.push(msg.clone());
            let cloned = s.clone();
            join_set.spawn(async move {
                cloned
                    .send(tonic::Request::new(SendReq { msg: Some(msg) }))
                    .await
            });
        }

        // we should only receive up to SESSION_BATCH_MAX
        let recv = s
            .recv(tonic::Request::new(RecvReq {
                src: Some(peer2.clone()),
            }))
            .await
            .unwrap();
        assert_stream(recv, &msgs[..SESSION_BATCH_MAX], SESSION_BATCH_MAX).await;

        // then get the rest of messages
        let recv = s
            .recv(tonic::Request::new(RecvReq {
                src: Some(peer2.clone()),
            }))
            .await
            .unwrap();
        assert_stream(recv, &msgs[SESSION_BATCH_MAX..], SESSION_BATCH_MAX).await;
        join_set.join_all().await;
    }

    #[tokio::test]
    async fn recv_first_then_send() {
        let (s, peer1, peer2) = setup();
        let msgs = vec![dummy_msg(peer1.clone(), peer2.clone(), 0)];

        let cloned_s = s.clone();
        let join = tokio::spawn(async move {
            cloned_s
                .recv(tonic::Request::new(RecvReq {
                    src: Some(peer2.clone()),
                }))
                .await
                .unwrap()
        });

        // let recv runs first since tokio test starts with single thread by default
        tokio::task::yield_now().await;
        s.send(tonic::Request::new(SendReq {
            msg: Some(msgs[0].clone()),
        }))
        .await
        .unwrap();

        let resp = join.await.unwrap();
        assert_stream(resp, &msgs, 1).await;
    }

    #[tokio::test]
    async fn recv_after_ttl() {
        let (s, peer1, peer2) = setup();
        let sent = vec![dummy_msg(peer1.clone(), peer2.clone(), 0)];
        let msg = sent[0].clone();

        let cloned = s.clone();
        tokio::spawn(async move {
            cloned.mailboxes.invalidate_all();
            cloned.mailboxes.run_pending_tasks().await;
            cloned
                .send(tonic::Request::new(SendReq { msg: Some(msg) }))
                .await
                .unwrap();
        });
        let received = s
            .recv(tonic::Request::new(RecvReq {
                src: Some(peer2.clone()),
            }))
            .await
            .unwrap();
        assert_stream(received, &sent, 1).await;
    }
}
