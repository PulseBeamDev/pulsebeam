//! Transfer a host-provisioned Chitchat socket into its transport API.
use std::net::SocketAddr;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use chitchat::ChitchatEnvelope;
use chitchat::transport::{RecvOutcome, SendOutcome, Socket, Transport};

// Shared-state exception: Chitchat opens through &self once. This control-side
// adapter transfers sole socket ownership; it is never accessed by shards.
#[allow(clippy::disallowed_types)]
pub struct BoundTransport(std::sync::Mutex<Option<Box<dyn Socket>>>);

impl BoundTransport {
    // The mutex only transfers the host-provisioned control-service socket.
    #[allow(clippy::disallowed_types)]
    pub fn new(socket: Box<dyn Socket>) -> Self {
        Self(std::sync::Mutex::new(Some(socket)))
    }
}

#[async_trait]
impl Transport for BoundTransport {
    async fn open(&self, listen_addr: SocketAddr) -> Result<Box<dyn Socket>> {
        let socket = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("transport poisoned"))?
            .take()
            .ok_or_else(|| anyhow::anyhow!("transport already opened"))?;
        ensure!(
            socket.local_addr()? == listen_addr,
            "gossip socket address differs from listen address"
        );
        Ok(Box::new(SupervisedSocket {
            inner: socket,
            send_failure: None,
        }))
    }
}

// Chitchat reports fatal receive failures, but ignores individual send failures.
// Carry a broken send into the next receive so its task watcher also reports it.
struct SupervisedSocket {
    inner: Box<dyn Socket>,
    send_failure: Option<String>,
}

#[async_trait]
impl Socket for SupervisedSocket {
    fn local_addr(&self) -> Result<SocketAddr> {
        self.inner.local_addr()
    }

    async fn send(&mut self, to: SocketAddr, envelope: ChitchatEnvelope) -> Result<SendOutcome> {
        match self.inner.send(to, envelope).await {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                self.send_failure = Some(error.to_string());
                Err(error)
            }
        }
    }

    async fn recv(&mut self) -> Result<RecvOutcome> {
        if let Some(error) = self.send_failure.take() {
            return Err(anyhow::anyhow!("fatal gossip send: {error}"));
        }
        self.inner.recv().await
    }
}
