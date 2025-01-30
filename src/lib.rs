pub mod pulsebeam {
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/pulsebeam.v1.rs"));
    }
}
use moka::sync::Cache;
use pulsebeam::v1::Message;
pub use pulsebeam::v1::{self as rpc};
use pulsebeam::v1::{PeerInfo, PrepareReq, PrepareResp, RecvReq, RecvResp, SendReq, SendResp};
use std::sync::Arc;
use tokio::time;
use twirp::async_trait::async_trait;

const SESSION_MAILBOX_CAPACITY: usize = 32;
const SESSION_POLL_TIMEOUT: time::Duration = time::Duration::from_secs(1200);
const SESSION_POLL_LATENCY_TOLERANCE: time::Duration = time::Duration::from_secs(5);
const SESSION_BATCH_TIMEOUT: time::Duration = time::Duration::from_millis(5);
const RESERVED_CONN_ID_DISCOVERY: u32 = 0;

type Channel = (flume::Sender<Message>, flume::Receiver<Message>);
pub struct Server {
    mailboxes: Cache<String, Channel>,
}

impl Server {
    pub fn new(max_capacity: u64) -> Arc<Self> {
        Arc::new(Self {
            // | peerA | peerB | description |
            // | t+0   | t+SESSION_POLL_TIMEOUT | let peerA messages drop, peerB will initiate |
            // | t+0   | t+SESSION_POLL_TIMEOUT-SESSION_POLL_LATENCY_TOLERANCE | peerB receives messages then it'll refresh cache on the next poll |
            mailboxes: Cache::builder()
                .time_to_idle(SESSION_POLL_TIMEOUT)
                .max_capacity(max_capacity)
                .build(),
        })
    }

    #[inline]
    fn get(&self, group_id: &str, peer_id: &str, conn_id: u32) -> Channel {
        let id = format!("{}:{}:{}", group_id, peer_id, conn_id);
        self.mailboxes
            .get_with_by_ref(&id, || flume::bounded(SESSION_MAILBOX_CAPACITY))
    }

    async fn recv_batch(&self, src: &PeerInfo) -> Vec<Message> {
        let (_, discovery_ch) = self.get(&src.group_id, &src.peer_id, RESERVED_CONN_ID_DISCOVERY);
        let (_, payload_ch) = self.get(&src.group_id, &src.peer_id, src.conn_id);
        let mut msgs = Vec::new();
        let mut poll_timeout = time::interval_at(
            time::Instant::now() + SESSION_POLL_TIMEOUT - SESSION_POLL_LATENCY_TOLERANCE,
            SESSION_BATCH_TIMEOUT,
        );

        loop {
            let mut msg: Option<Message> = None;
            tokio::select! {
                Ok(m) = discovery_ch.recv_async() => {
                    msg = Some(m);
                    poll_timeout.reset();
                }
                Ok(m) = payload_ch.recv_async() => {
                    msg = Some(m);
                    poll_timeout.reset();
                }
                _ = poll_timeout.tick() => {}
            }

            if let Some(msg) = msg {
                msgs.push(msg);
            } else {
                return msgs;
            }
        }
    }
}

#[async_trait]
impl rpc::Tunnel for Server {
    async fn prepare(
        &self,
        _ctx: twirp::Context,
        _req: PrepareReq,
    ) -> Result<PrepareResp, twirp::TwirpErrorResponse> {
        Ok(PrepareResp {
            ice_servers: vec![],
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

        let (ch, _) = self.get(&dst.group_id, &dst.peer_id, dst.conn_id);
        ch.send(msg)
            .map_err(|err| twirp::internal(err.to_string()))?;

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
        let s = Server::new(10_000);
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
        s.send(dummy_ctx(), SendReq {
            msg: Some(msgs[0].clone()),
        })
        .await
        .unwrap();

        let resp = s
            .recv(dummy_ctx(), RecvReq {
                src: Some(peer2.clone()),
            })
            .await
            .unwrap();

        assert_msgs(&resp.msgs, &msgs);
    }

    #[tokio::test]
    async fn recv_first_then_send() {
        let (s, peer1, peer2) = setup();
        let msgs = vec![dummy_msg(peer1.clone(), peer2.clone(), 0)];

        let cloned_s = Arc::clone(&s);
        let join = tokio::spawn(async move {
            cloned_s
                .recv(dummy_ctx(), RecvReq {
                    src: Some(peer2.clone()),
                })
                .await
                .unwrap()
        });

        // let recv runs first since tokio test starts with single thread by default
        tokio::task::yield_now().await;
        s.send(dummy_ctx(), SendReq {
            msg: Some(msgs[0].clone()),
        })
        .await
        .unwrap();

        let resp = join.await.unwrap();
        assert_msgs(&resp.msgs, &msgs);
    }

    #[tokio::test]
    async fn recv_after_timeout() {
        let (s, peer1, peer2) = setup();
        let msgs = vec![dummy_msg(peer1.clone(), peer2.clone(), 0)];
        s.send(dummy_ctx(), SendReq {
            msg: Some(msgs[0].clone()),
        })
        .await
        .unwrap();

        s.mailboxes.invalidate_all();
        let resp = time::timeout(
            time::Duration::from_millis(5),
            s.recv(dummy_ctx(), RecvReq {
                src: Some(peer2.clone()),
            }),
        )
        .await;

        // expect timeout to hit because there's no message
        assert!(resp.is_err());
    }
}
