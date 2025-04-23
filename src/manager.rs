use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use axum_extra::{TypedHeader, headers::ContentType};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};

use crate::{
    egress::EgressHandle,
    ingress::IngressHandle,
    peer::{PeerError, PeerHandle},
};

#[derive(thiserror::Error, Debug)]
pub enum ManagerError {
    #[error(transparent)]
    Peer(PeerError),

    #[error("unknown error: {0}")]
    Unknown(String),
}

impl IntoResponse for ManagerError {
    fn into_response(self) -> Response {
        let status = match self {
            ManagerError::Peer(_) => StatusCode::UNAUTHORIZED,
            ManagerError::Unknown(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}

#[derive(Debug)]
pub enum ManagerMessage {
    Offer(String, oneshot::Sender<Result<String, PeerError>>),
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
            ManagerMessage::Offer(offer, resp_tx) => {
                let res = PeerHandle::spawn(offer);
                let answer = match res {
                    Ok((handle, answer)) => Ok(answer),
                    Err(err) => Err(err),
                };
                let _ = resp_tx.send(answer);
            }
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
}

#[axum::debug_handler]
async fn spawn_peer(
    State(handle): State<ManagerHandle>,
    TypedHeader(content_type): TypedHeader<ContentType>,
    offer: String,
) -> Result<String, ManagerError> {
    // TODO: validate content_type = "application/sdp"

    let (resp_tx, resp_rx) = oneshot::channel();
    handle
        .sender
        .send(ManagerMessage::Offer(offer, resp_tx))
        .await
        .map_err(|err| ManagerError::Unknown(err.to_string()))?;
    let answer = resp_rx
        .await
        .map_err(|err| ManagerError::Unknown(err.to_string()))?
        .map_err(ManagerError::Peer)?;
    Ok(answer)
}

pub fn router(handle: ManagerHandle) -> Router {
    Router::new().route("/", get(spawn_peer)).with_state(handle)
}
