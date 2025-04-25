use std::time::Duration;

use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use axum_extra::{TypedHeader, headers::ContentType};
use tokio::{net::TcpListener, sync::mpsc};

use crate::{
    egress::EgressHandle,
    ingress::IngressHandle,
    peer::{PeerActor, PeerError, PeerInfo},
};

#[derive(thiserror::Error, Debug)]
pub enum ManagerError {
    #[error(transparent)]
    Peer(PeerError),

    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("unknown error: {0}")]
    Unknown(String),
}

impl IntoResponse for ManagerError {
    fn into_response(self) -> Response {
        let status = match self {
            ManagerError::Peer(_) => StatusCode::UNAUTHORIZED,
            ManagerError::Unknown(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ManagerError::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, self.to_string()).into_response()
    }
}

#[derive(Debug)]
pub enum ManagerMessage {
    AddPeer(PeerActor),
}

pub struct ManagerActor {
    pub ingress: IngressHandle,
    pub egress: EgressHandle,
    pub listener: TcpListener,
}

impl ManagerActor {
    async fn run(self, mut receiver: mpsc::Receiver<ManagerMessage>) {
        while let Some(msg) = receiver.recv().await {
            self.handle_message(msg);
        }
    }

    fn handle_message(&self, msg: ManagerMessage) {
        match msg {
            ManagerMessage::AddPeer(peer) => {}
        }
    }
}

#[derive(Clone)]
pub struct ManagerHandle {
    sender: mpsc::Sender<ManagerMessage>,
}

impl ManagerHandle {
    pub fn spawn(actor: ManagerActor) -> Self {
        let (sender, receiver) = mpsc::channel(1);
        let handle = Self { sender };
        tokio::spawn(actor.run(receiver));
        handle
    }

    pub async fn add_peer(&self, peer: PeerActor) -> Result<(), ManagerError> {
        self.sender
            .send_timeout(ManagerMessage::AddPeer(peer), Duration::from_millis(100))
            .await
            .map_err(|_| ManagerError::ServiceUnavailable)
    }
}

#[axum::debug_handler]
async fn spawn_peer(
    Query(peer): Query<PeerInfo>,
    State(handle): State<ManagerHandle>,
    TypedHeader(content_type): TypedHeader<ContentType>,
    offer: String,
) -> Result<String, ManagerError> {
    // TODO: validate content_type = "application/sdp"

    let (peer, answer) = PeerActor::offer(peer, &offer).map_err(ManagerError::Peer)?;
    handle.add_peer(peer).await?;

    Ok(answer)
}

pub fn router(handle: ManagerHandle) -> Router {
    Router::new().route("/", get(spawn_peer)).with_state(handle)
}
