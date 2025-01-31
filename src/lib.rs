pub mod pulsebeam {
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/pulsebeam.v1.rs"));
    }
}
use moka::future::Cache;
pub use pulsebeam::v1::{self as rpc};
use pulsebeam::v1::{IceServer, Message};
use pulsebeam::v1::{PeerInfo, PrepareReq, PrepareResp, RecvReq, RecvResp, SendReq, SendResp};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::time;
use twirp::async_trait::async_trait;

const SESSION_POLL_TIMEOUT: time::Duration = time::Duration::from_secs(1200);
const SESSION_POLL_LATENCY_TOLERANCE: time::Duration = time::Duration::from_secs(5);
const SESSION_BATCH_TIMEOUT: time::Duration = time::Duration::from_millis(5);
const RESERVED_CONN_ID_DISCOVERY: u32 = 0;

type Mailbox = (flume::Sender<rpc::Message>, flume::Receiver<rpc::Message>);
pub struct Server {
    mailboxes: Cache<String, Mailbox, ahash::RandomState>,
}

pub struct ServerConfig {
    pub max_capacity: u64,
}

impl Server {
    pub fn new(cfg: ServerConfig) -> Arc<Self> {
        Arc::new(Self {
            // | peerA | peerB | description |
            // | t+0   | t+SESSION_POLL_TIMEOUT | let peerA messages drop, peerB will initiate |
            // | t+0   | t+SESSION_POLL_TIMEOUT-SESSION_POLL_LATENCY_TOLERANCE | peerB receives messages then it'll refresh cache on the next poll |
            mailboxes: Cache::builder()
                .time_to_idle(SESSION_POLL_TIMEOUT)
                .max_capacity(cfg.max_capacity)
                .build_with_hasher(ahash::RandomState::default()),
        })
    }

    #[inline]
    async fn get(&self, group_id: &str, peer_id: &str, conn_id: u32) -> Mailbox {
        let id = format!("{}:{}:{}", group_id, peer_id, conn_id);
        self.mailboxes
            .get_with_by_ref(&id, async { flume::bounded(0) })
            .await
    }

    async fn recv_batch(&self, src: &PeerInfo) -> Vec<Message> {
        let (_, discovery) = self
            .get(&src.group_id, &src.peer_id, RESERVED_CONN_ID_DISCOVERY)
            .await;
        let (_, payload) = self.get(&src.group_id, &src.peer_id, src.conn_id).await;
        let mut set = HashSet::with_capacity_and_hasher(16, ahash::RandomState::default());
        let mut poll_timeout = time::interval_at(
            time::Instant::now() + SESSION_POLL_TIMEOUT - SESSION_POLL_LATENCY_TOLERANCE,
            SESSION_BATCH_TIMEOUT,
        );

        loop {
            let mut msg: Option<Message> = None;
            tokio::select! {
                m = discovery.recv_async() => {
                    msg = m.ok();
                    poll_timeout.reset();
                }
                m = payload.recv_async() => {
                    msg= m.ok();
                    poll_timeout.reset();
                }
                _ = poll_timeout.tick() => {}
            }

            if let Some(msgs) = msg {
                set.insert(msgs);
            } else {
                break;
            }
        }

        set.into_iter().collect()
    }
}

#[async_trait]
impl rpc::Tunnel for Server {
    async fn prepare(
        &self,
        _ctx: twirp::Context,
        _req: PrepareReq,
    ) -> Result<PrepareResp, twirp::TwirpErrorResponse> {
        // WARNING: PLEASE READ THIS FIRST!
        // By default, OSS/self-hosting only provides a public STUN server.
        // You must provide your own TURN and STUN services.
        // TURN is required in some network condition.
        // Public TURN and STUN services are UNRELIABLE.
        Ok(PrepareResp {
            ice_servers: vec![IceServer {
                urls: vec![String::from("stun:stun.l.google.com:19302")],
                username: None,
                credential: None,
            }],
        })
    }

    async fn send(
        &self,
        _ctx: twirp::Context,
        req: SendReq,
    ) -> Result<SendResp, twirp::TwirpErrorResponse> {
        let msg = req.msg.ok_or(twirp::invalid_argument("msg is required"))?;
        let hdr = msg
            .header
            .as_ref()
            .ok_or(twirp::invalid_argument("header is required"))?;
        let dst = hdr
            .dst
            .as_ref()
            .ok_or(twirp::invalid_argument("dst is required"))?;

        let (ch, _) = self.get(&dst.group_id, &dst.peer_id, dst.conn_id).await;
        ch.send_async(msg)
            .await
            .map_err(|err| twirp::aborted(err.to_string()))?;

        Ok(SendResp {})
    }

    async fn recv(
        &self,
        _ctx: twirp::Context,
        req: RecvReq,
    ) -> Result<RecvResp, twirp::TwirpErrorResponse> {
        let src = req.src.ok_or(twirp::invalid_argument("src is required"))?;

        if src.conn_id == RESERVED_CONN_ID_DISCOVERY {
            return Err(twirp::invalid_argument(
                "conn_id must not use reserved conn ids",
            ));
        }

        let msgs = self.recv_batch(&src).await;
        Ok(RecvResp { msgs })
    }
}

#[cfg(test)]
mod test {
    use std::iter::zip;
    use std::sync::Mutex;

    use super::*;
    use rpc::{MessageHeader, MessagePayload, Tunnel};

    fn dummy_ctx() -> twirp::Context {
        twirp::Context::new(
            http::Extensions::new(),
            Arc::new(Mutex::new(http::Extensions::new())),
        )
    }

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

    fn assert_msgs(received: &Vec<Message>, sent: &Vec<Message>) {
        assert_eq!(received.len(), sent.len());
        let pairs = zip(received, sent);
        for (a, b) in pairs.into_iter() {
            assert_eq!(a, b);
        }
    }

    fn setup() -> (Arc<Server>, PeerInfo, PeerInfo) {
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
            s.send(
                dummy_ctx(),
                SendReq {
                    msg: Some(msgs[0].clone()),
                },
            ),
            s.recv(
                dummy_ctx(),
                RecvReq {
                    src: Some(peer2.clone()),
                },
            )
        );

        send.unwrap();
        let resp = recv.unwrap();
        assert_msgs(&resp.msgs, &msgs);
    }

    #[tokio::test]
    async fn recv_first_then_send() {
        let (s, peer1, peer2) = setup();
        let msgs = vec![dummy_msg(peer1.clone(), peer2.clone(), 0)];

        let cloned_s = Arc::clone(&s);
        let join = tokio::spawn(async move {
            cloned_s
                .recv(
                    dummy_ctx(),
                    RecvReq {
                        src: Some(peer2.clone()),
                    },
                )
                .await
                .unwrap()
        });

        // let recv runs first since tokio test starts with single thread by default
        tokio::task::yield_now().await;
        s.send(
            dummy_ctx(),
            SendReq {
                msg: Some(msgs[0].clone()),
            },
        )
        .await
        .unwrap();

        let resp = join.await.unwrap();
        assert_msgs(&resp.msgs, &msgs);
    }

    #[tokio::test]
    async fn recv_after_ttl() {
        let (s, peer1, peer2) = setup();
        let sent = vec![dummy_msg(peer1.clone(), peer2.clone(), 0)];
        let msg = sent[0].clone();

        let cloned = Arc::clone(&s);
        tokio::spawn(async move {
            cloned.mailboxes.invalidate_all();
            cloned.mailboxes.run_pending_tasks().await;
            cloned
                .send(dummy_ctx(), SendReq { msg: Some(msg) })
                .await
                .unwrap();
        });
        let received = s.recv_batch(&peer2).await;
        assert_eq!(received.len(), 0);

        let received = s.recv_batch(&peer2).await;
        assert_msgs(&received, &sent);
    }
}
