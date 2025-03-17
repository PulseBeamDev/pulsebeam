use crate::proto::signaling_server::Signaling;
use crate::proto::{self, PeerInfo};
use axum::extract::{Path, State};
use axum::routing::get;
use axum::Json;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;
use tracing::field::valuable;
use valuable::Enumerable;

use crate::manager::{GroupId, Manager, ManagerConfig, PeerStats};
const RESERVED_CONN_ID_DISCOVERY: u32 = 0;
const RECV_STREAM_BUFFER: usize = 8;
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(45);

#[derive(Clone)]
pub struct Server {
    manager: Manager,
}

pub type MessageStream = Pin<Box<dyn Stream<Item = proto::Message> + Send>>;

impl Server {
    pub fn spawn(token: CancellationToken, cfg: ManagerConfig) -> Self {
        let manager = Manager::spawn(token, cfg);
        Self { manager }
    }

    pub async fn insert_recv_stream(&self, src: PeerInfo) -> MessageStream {
        let conn = self.manager.allocate(src).await;
        let payload_stream = ReceiverStream::new(conn);

        let repeat = std::iter::repeat(proto::Message {
            header: None,
            payload: Some(proto::MessagePayload {
                payload_type: Some(proto::message_payload::PayloadType::Ping(proto::Ping {})),
            }),
        });
        let keep_alive = tokio_stream::iter(repeat).throttle(KEEP_ALIVE_INTERVAL);
        let merged = keep_alive.merge(payload_stream);
        Box::pin(merged) as MessageStream
    }

    pub fn query_routes(&self) -> axum::Router {
        axum::Router::new()
            .route("/{group_id}", get(handle_group_query))
            .with_state(self.clone())
    }
}

#[axum::debug_handler]
async fn handle_group_query(
    Path(group_id): Path<GroupId>,
    State(server): State<Server>,
) -> Json<Vec<(PeerInfo, PeerStats)>> {
    let result = server.manager.index.select_group(group_id);
    Json(result)
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
        let hdr = msg
            .header
            .as_mut()
            .ok_or(tonic::Status::invalid_argument("header is required"))?;
        let dst = hdr
            .dst
            .as_mut()
            .ok_or(tonic::Status::invalid_argument("dst is required"))?;
        let src = hdr
            .src
            .as_ref()
            .ok_or(tonic::Status::invalid_argument("src is required"))?;

        tracing::trace!(
            "send: {} -> {} ({:?})",
            src,
            dst,
            msg.payload
                .as_ref()
                .and_then(|p| p.payload_type.as_ref())
                .map(|p| p.variant().name().to_string())
        );

        if src.group_id == dst.group_id && src.peer_id == dst.peer_id {
            return Err(tonic::Status::invalid_argument(
                "detected a loopback, dst must be different than src",
            ));
        }

        // TODO: use a different RPC for connecting?
        let peer = if dst.conn_id == RESERVED_CONN_ID_DISCOVERY {
            let start = dst.clone();
            let mut end = dst.clone();
            end.conn_id = u32::MAX;

            let selected = self
                .manager
                .index
                .select_one(start..=end)
                .ok_or(tonic::Status::not_found("peer_id is not available"))?;
            dst.conn_id = selected.conn_id;
            self.manager
                .conns
                .get(&selected)
                .await
                .ok_or(tonic::Status::not_found("peer_id is not available"))?
        } else {
            self.manager
                .conns
                .get(dst)
                .await
                .ok_or(tonic::Status::not_found("peer_id is not available"))?
        };

        peer.send(msg)
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

        let manager = self.manager.clone();
        let peer = src.clone();

        let mut payload = self.insert_recv_stream(src).await;
        let (tx, rx) = mpsc::channel(RECV_STREAM_BUFFER);
        tokio::spawn(async move {
            while let Some(item) = payload.next().await {
                let resp = proto::RecvResp { msg: Some(item) };
                match tx.send(Result::<_, tonic::Status>::Ok(resp)).await {
                    Ok(_) => {
                        // item (server response) was queued to be send to client
                    }
                    Err(_item) => {
                        // output_stream was build from rx and both are dropped
                        break;
                    }
                }
            }

            tracing::info!(
                peer = valuable(&peer),
                "detected connection dropped, removing peer"
            );
            manager.conns.invalidate(&peer).await;
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(tonic::Response::new(
            Box::pin(output_stream) as Self::RecvStream
        ))
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

    async fn stream_to_vec(received: MessageStream, take: usize) -> Vec<Message> {
        received
            .filter(|m| m.header.is_some())
            .take(take)
            .collect()
            .await
    }

    fn setup() -> (Server, PeerInfo, PeerInfo) {
        let s = Server::spawn(CancellationToken::new(), ManagerConfig::default());
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
            stream_to_vec(s.insert_recv_stream(peer2).await, 1),
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
            stream_to_vec(s.insert_recv_stream(peer2).await, 2),
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
            let recv_stream = cloned_s.insert_recv_stream(peer2.clone()).await;
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
        let results = s.manager.index.select_group(peer1.group_id.clone());
        assert_eq!(results.len(), 0);

        let _stream1 = s.insert_recv_stream(peer1.clone()).await;
        let _stream2 = s.insert_recv_stream(peer2.clone()).await;
        tokio::task::yield_now().await;
        let mut results: Vec<PeerInfo> = s
            .manager
            .index
            .select_group(peer1.group_id.clone())
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        println!("{:?}", results);
        assert_eq!(results.len(), 2);
        results.sort();
        assert_eq!(results[0].peer_id, peer1.peer_id);
        assert_eq!(results[0].conn_id, peer1.conn_id);
        assert_eq!(results[1].peer_id, peer2.peer_id);
        assert_eq!(results[1].conn_id, peer2.conn_id);
    }
}
