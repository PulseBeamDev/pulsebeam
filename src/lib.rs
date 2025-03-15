pub mod pulsebeam {
    pub mod v1 {
        tonic::include_proto!("pulsebeam.v1");
    }
}

use moka::future::{Cache, CacheBuilder};
use pulsebeam::v1::signaling_server::Signaling;
pub use pulsebeam::v1::signaling_server::SignalingServer;
use pulsebeam::v1::{
    IceServer, Message, PeerInfo, PrepareReq, PrepareResp, RecvResp, SendReq, SendResp,
};
use std::sync::Arc;
use std::{ops::Deref, pin::Pin};
use tokio_stream::{Stream, StreamExt};
use tracing::field::valuable;

const RESERVED_CONN_ID_DISCOVERY: u32 = 0;
type Mailbox = (flume::Sender<Message>, flume::Receiver<Message>);
type GroupId = String;
type PeerId = String;
type PeerConn = (PeerId, u32);
type Group = Cache<PeerConn, Mailbox, ahash::RandomState>;

pub struct ServerConfig {
    pub max_groups: u64,
    pub max_peers_per_group: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_groups: 65536,
            max_peers_per_group: 16,
        }
    }
}

#[derive(Clone)]
pub struct Server {
    groups: Cache<GroupId, Group, ahash::RandomState>,
    cfg: Arc<ServerConfig>,
}

impl Default for Server {
    fn default() -> Self {
        Self::new(ServerConfig::default())
    }
}

impl Server {
    pub fn new(cfg: ServerConfig) -> Self {
        Self {
            groups: CacheBuilder::new(cfg.max_groups)
                .build_with_hasher(ahash::RandomState::default()),
            cfg: Arc::new(cfg),
        }
    }

    async fn get(&self, info: &PeerInfo) -> Mailbox {
        let peers = self
            .groups
            .get_with_by_ref(&info.group_id, async {
                CacheBuilder::new(self.cfg.max_peers_per_group)
                    .build_with_hasher(ahash::RandomState::default())
            })
            .await;

        let peer_conn = (info.peer_id.clone(), info.conn_id);
        peers
            .get_with_by_ref(&peer_conn, async { flume::bounded(0) })
            .await
    }

    pub async fn query_peers(&self, group_id: &str) -> Vec<PeerConn> {
        let result = self.groups.get(group_id).await;
        if let Some(peers) = result {
            peers.iter().map(|(k, _)| k.deref().clone()).collect()
        } else {
            vec![]
        }
    }

    pub async fn recv_stream(&self, src: &PeerInfo) -> RecvStream {
        let discovery_info = PeerInfo {
            group_id: src.group_id.clone(),
            peer_id: src.peer_id.clone(),
            conn_id: RESERVED_CONN_ID_DISCOVERY,
        };
        let (_, discovery) = self.get(&discovery_info).await;
        let (_, payload) = self.get(src).await;

        let discovery_stream = discovery.into_stream().map(|msg| {
            tracing::trace!(msg = valuable(&msg), "discovery stream");
            Ok(RecvResp { msg: Some(msg) })
        });
        let payload_stream = payload.into_stream().map(|msg| {
            tracing::trace!(msg = valuable(&msg), "payload stream");
            Ok(RecvResp { msg: Some(msg) })
        });

        let merged = discovery_stream.merge(payload_stream);
        Box::pin(merged) as RecvStream
    }
}

pub type RecvStream = Pin<Box<dyn Stream<Item = Result<RecvResp, tonic::Status>> + Send>>;

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
        tracing::trace!(msg = valuable(&msg), "send");
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

        tracing::trace!(src = valuable(&src), "recv");
        let stream = self.recv_stream(&src).await;
        Ok(tonic::Response::new(stream))
    }
}

#[cfg(test)]
mod test {
    use std::iter::zip;

    use super::*;
    use pulsebeam::v1::{MessageHeader, MessagePayload};

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

    async fn stream_to_vec(received: RecvStream, take: usize) -> Vec<Message> {
        received
            .filter_map(|r| r.ok())
            .filter_map(|r| r.msg)
            .take(take)
            .collect()
            .await
    }

    fn setup() -> (Server, PeerInfo, PeerInfo) {
        let s = Server::default();
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
    async fn recv_normal_single() {
        let (s, peer1, peer2) = setup();
        let msgs = vec![dummy_msg(peer1.clone(), peer2.clone(), 0)];
        let (send, recv) = tokio::join!(
            s.send(tonic::Request::new(SendReq {
                msg: Some(msgs[0].clone()),
            })),
            stream_to_vec(s.recv_stream(&peer2).await, 1),
        );

        send.unwrap();
        assert_msgs(&recv, &msgs);
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
            stream_to_vec(s.recv_stream(&peer2).await, 2),
        );

        send1.unwrap();
        send2.unwrap();
        assert_msgs(&recv, &msgs);
    }

    #[tokio::test]
    async fn recv_first_then_send() {
        let (s, peer1, peer2) = setup();
        let msgs = vec![dummy_msg(peer1.clone(), peer2.clone(), 0)];

        let cloned_s = s.clone();
        let join = tokio::spawn(async move {
            let recv_stream = cloned_s.recv_stream(&peer2.clone()).await;
            stream_to_vec(recv_stream, 1).await
        });

        // let recv runs first since tokio test starts with single thread by default
        tokio::task::yield_now().await;
        s.send(tonic::Request::new(SendReq {
            msg: Some(msgs[0].clone()),
        }))
        .await
        .unwrap();

        let resp = join.await.unwrap();
        assert_msgs(&resp, &msgs);
    }

    #[tokio::test]
    async fn query_peers() {
        let (s, peer1, peer2) = setup();
        let results = s.query_peers(&peer1.group_id).await;
        assert_eq!(results.len(), 0);

        let _stream1 = s.recv_stream(&peer1).await;
        let _stream2 = s.recv_stream(&peer2).await;
        let mut results = s.query_peers(&peer1.group_id).await;
        println!("{:?}", results);
        assert_eq!(results.len(), 4);
        results.sort();
        assert_eq!(results[0].0, peer1.peer_id);
        assert_eq!(results[0].1, RESERVED_CONN_ID_DISCOVERY);
        assert_eq!(results[1].0, peer1.peer_id);
        assert_eq!(results[1].1, peer1.conn_id);

        assert_eq!(results[2].0, peer2.peer_id);
        assert_eq!(results[2].1, RESERVED_CONN_ID_DISCOVERY);
        assert_eq!(results[3].0, peer2.peer_id);
        assert_eq!(results[3].1, peer2.conn_id);
    }
}
