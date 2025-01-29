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

const SESSION_TTL: time::Duration = time::Duration::from_secs(3);
const SESSION_CAPACITY: u64 = 10_000;
const SESSION_MAILBOX_CAPACITY: usize = 32;
const SESSION_POLL_TIMEOUT: time::Duration = time::Duration::from_secs(1200);
const SESSION_BATCH_TIMEOUT: time::Duration = time::Duration::from_millis(5);
const RESERVED_CONN_ID_DISCOVERY: u32 = 0;

type Channel = (flume::Sender<Message>, flume::Receiver<Message>);
pub struct Server {
    mailboxes: Cache<String, Channel>,
}

impl Server {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mailboxes: Cache::builder()
                .time_to_live(SESSION_TTL)
                .max_capacity(SESSION_CAPACITY)
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
            time::Instant::now() + SESSION_POLL_TIMEOUT,
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
