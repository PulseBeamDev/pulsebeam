use crate::evicter::Evicter;
use crate::proto::signaling_server::Signaling;
use crate::proto::{self, PeerInfo};
use std::pin::Pin;
use std::time::Duration;
use tokio_stream::{Stream, StreamExt};
use tracing::field::valuable;

use crate::manager::{Manager, ManagerConfig, PeerConn};
const RESERVED_CONN_ID_DISCOVERY: u32 = 0;

#[derive(Clone)]
pub struct Server {
    manager: Manager,
    evicter: Evicter,
}

impl Default for Server {
    fn default() -> Self {
        Self::new(ManagerConfig {
            max_groups: 65536,
            max_peers_per_group: 16,
        })
    }
}

impl Server {
    pub fn new(cfg: ManagerConfig) -> Self {
        let manager = Manager::new(cfg);
        // TODO: configurable timeout
        let evicter = Evicter::new(manager.clone(), Duration::from_secs(30));
        Self { manager, evicter }
    }

    pub async fn recv_stream(&self, src: PeerInfo) -> RecvStream {
        let group = self.manager.get(src.group_id).await;
        let conn = PeerConn {
            peer_id: src.peer_id,
            conn_id: src.conn_id,
        };
        let mailbox = group.get(conn).await;
        let payload_stream = mailbox.1.into_stream().map(|msg| {
            tracing::trace!(msg = valuable(&msg), "payload stream");
            Ok(proto::RecvResp { msg: Some(msg) })
        });
        Box::pin(payload_stream) as RecvStream
    }
}

pub type RecvStream = Pin<Box<dyn Stream<Item = Result<proto::RecvResp, tonic::Status>> + Send>>;

#[tonic::async_trait]
impl Signaling for Server {
    async fn prepare(
        &self,
        _req: tonic::Request<proto::PrepareReq>,
    ) -> Result<tonic::Response<proto::PrepareResp>, tonic::Status> {
        // WARNING: PLEASE READ THIS FIRST!
        // By default, OSS/self-hosting only provides a public STUN server.
        // You must provide your own TURN and STUN services.
        // TURN is required in some network condition.
        // Public TURN and STUN services are UNRELIABLE.
        Ok(tonic::Response::new(proto::PrepareResp {
            ice_servers: vec![proto::IceServer {
                urls: vec![String::from("stun:stun.l.google.com:19302")],
                username: None,
                credential: None,
            }],
        }))
    }

    async fn send(
        &self,
        req: tonic::Request<proto::SendReq>,
    ) -> Result<tonic::Response<proto::SendResp>, tonic::Status> {
        let mut msg = req
            .into_inner()
            .msg
            .ok_or(tonic::Status::invalid_argument("msg is required"))?;
        tracing::trace!(msg = valuable(&msg), "send");
        let hdr = msg
            .header
            .as_mut()
            .ok_or(tonic::Status::invalid_argument("header is required"))?;
        let mut dst = hdr
            .dst
            .as_mut()
            .ok_or(tonic::Status::invalid_argument("dst is required"))?
            .clone();

        let group = self.manager.get(dst.group_id).await;

        // TODO: use a different RPC for connecting?
        let mailbox = if dst.conn_id == RESERVED_CONN_ID_DISCOVERY {
            let selected = group
                .select_one(dst.peer_id)
                .await
                .ok_or(tonic::Status::out_of_range("peer id not present"))?;
            dst.conn_id = selected.0.conn_id;
            selected.1
        } else {
            let conn = PeerConn {
                peer_id: dst.peer_id,
                conn_id: dst.conn_id,
            };
            group.get(conn).await
        };
        mailbox
            .0
            .send_async(msg)
            .await
            .map_err(|err| tonic::Status::aborted(err.to_string()))?;

        Ok(tonic::Response::new(proto::SendResp {}))
    }

    type RecvStream = RecvStream;
    async fn recv(
        &self,
        req: tonic::Request<proto::RecvReq>,
    ) -> std::result::Result<tonic::Response<Self::RecvStream>, tonic::Status> {
        let src = req
            .into_inner()
            .src
            .ok_or(tonic::Status::invalid_argument("src is required"))?;

        tracing::trace!(src = valuable(&src), "recv");

        let payload = self.recv_stream(src).await;
        Ok(tonic::Response::new(payload))
    }
}

#[cfg(test)]
mod test {
    use std::iter::zip;

    use super::*;
    use proto::*;

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
            stream_to_vec(s.recv_stream(peer2).await, 1),
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
            stream_to_vec(s.recv_stream(peer2).await, 2),
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
            let recv_stream = cloned_s.recv_stream(peer2.clone()).await;
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
        let group = s.manager.get(peer1.group_id.clone()).await;
        let results = group.collect().await;
        assert_eq!(results.len(), 0);

        let _stream1 = s.recv_stream(peer1.clone()).await;
        let _stream2 = s.recv_stream(peer2.clone()).await;
        let mut results = group.collect().await;
        println!("{:?}", results);
        assert_eq!(results.len(), 2);
        results.sort();
        assert_eq!(results[0].peer_id, peer1.peer_id);
        assert_eq!(results[0].conn_id, peer1.conn_id);
        assert_eq!(results[1].peer_id, peer2.peer_id);
        assert_eq!(results[1].conn_id, peer2.conn_id);
    }
}
