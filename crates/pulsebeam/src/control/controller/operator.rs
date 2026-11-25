use std::{
    fs, io,
    num::NonZeroU64,
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixListener as StdListener,
    },
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use pulsebeam_runtime::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixListener,
    sync::CancellationToken,
    time::timeout,
};

use pulsebeam_core::{
    auth::{KeyRegistry, MAX_COMPACT_TOKEN_LEN, authenticate_participant_token},
    identity::NodeId,
};
use pulsebeam_runtime::sync::oneshot;
use serde::{Deserialize, Serialize};

use super::{
    ControllerCommand, ControllerError, ControllerSender, NativeResourceRequest,
    PublisherHandoffCommand, StandbyControlCommand,
    ownership::receipt::standby::{Source, wire::DelegationImage},
    publisher_handoffs::Status as HandoffStatus,
    standby_egress::{Operation, Receipt, Request as StandbyRequest},
};
use crate::{
    cluster::handoff::Owner,
    entity::{ConnectionId, PublicationId, PublicationKey, RoomId, TrackId},
};

const REQUEST_BYTES: usize = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Publication {
    Audio(PublicationId),
    Video(PublicationId),
}

impl From<Publication> for PublicationKey {
    fn from(publication: Publication) -> Self {
        match publication {
            Publication::Audio(id) => Self::Audio(id),
            Publication::Video(id) => Self::Video(id),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Encoder {
    node: NodeId,
    generation: NonZeroU64,
    connection: ConnectionId,
    publication: Publication,
    track: TrackId,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", tag = "operation", deny_unknown_fields)]
enum Action {
    Grant {
        publication: Publication,
        encoder: Encoder,
        expires_at: u64,
    },
    Advertise {
        delegation: DelegationImage,
    },
    Select {
        publication: Publication,
        revision: NonZeroU64,
        delegation: Option<DelegationImage>,
    },
    PrepareHandoff {
        room: RoomId,
    },
    Handoff {
        successor: Owner,
        sequence: NonZeroU64,
    },
    HandoffStatus {
        room: RoomId,
        successor: Owner,
        sequence: NonZeroU64,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    token: String,
    connection: Option<ConnectionId>,
    action: Action,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
enum Response {
    Granted { delegation: Box<DelegationImage> },
    Queued,
    HandoffPrepared { successor: Owner },
    HandoffCompleted,
    HandoffPending,
    HandoffNotFound,
    InvalidRequest,
    Unauthorized,
    Unavailable,
    Rejected,
    OutcomeUnknown,
}

impl From<ControllerError> for Response {
    fn from(error: ControllerError) -> Self {
        match error {
            ControllerError::AuthorizationRejected | ControllerError::AuthorizationExpired => {
                Self::Unauthorized
            }
            ControllerError::ServiceUnavailable => Self::Unavailable,
            _ => Self::Rejected,
        }
    }
}

async fn dispatch(bytes: &[u8], registry: &KeyRegistry, sender: &ControllerSender) -> Response {
    if bytes.len() > REQUEST_BYTES {
        return Response::InvalidRequest;
    }
    let Ok(Request {
        token,
        connection,
        action,
    }) = serde_json::from_slice(bytes)
    else {
        return Response::InvalidRequest;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return Response::Unavailable;
    };
    if token.len() > MAX_COMPACT_TOKEN_LEN {
        return Response::Unauthorized;
    }
    let Ok(principal) = authenticate_participant_token(registry, &token, now.as_secs()) else {
        return Response::Unauthorized;
    };
    if let Action::PrepareHandoff { room } = action {
        if connection.is_some() {
            return Response::InvalidRequest;
        }
        let (reply, result) = oneshot::channel();
        let command = PublisherHandoffCommand::prepare(principal, room, reply);
        if sender
            .try_send(ControllerCommand::PublisherHandoff(command))
            .is_err()
        {
            return Response::Unavailable;
        }
        return match result.await {
            Ok(Ok(successor)) => Response::HandoffPrepared { successor },
            Ok(Err(error)) => error.into(),
            Err(_) => Response::Unavailable,
        };
    }
    let Some(connection_id) = connection else {
        return Response::InvalidRequest;
    };
    let resource = NativeResourceRequest {
        principal,
        connection_id,
    };
    let (publication, operation, grant_source) = match action {
        Action::PrepareHandoff { .. } => return Response::InvalidRequest,
        Action::Handoff {
            successor,
            sequence,
        } => {
            let (reply, result) = oneshot::channel();
            if sender
                .try_send(ControllerCommand::PublisherHandoff(
                    PublisherHandoffCommand::new(resource, successor, sequence, reply),
                ))
                .is_err()
            {
                return Response::Unavailable;
            }
            return match result.await {
                Ok(Ok(())) => Response::HandoffCompleted,
                Ok(Err(ControllerError::ServiceUnavailable)) | Err(_) => Response::OutcomeUnknown,
                Ok(Err(error)) => error.into(),
            };
        }
        Action::HandoffStatus {
            room,
            successor,
            sequence,
        } => {
            let (reply, result) = oneshot::channel();
            let command =
                PublisherHandoffCommand::inspect(resource, room, successor, sequence, reply);
            if sender
                .try_send(ControllerCommand::PublisherHandoff(command))
                .is_err()
            {
                return Response::Unavailable;
            }
            return match result.await {
                Ok(Ok(HandoffStatus::Pending)) => Response::HandoffPending,
                Ok(Ok(HandoffStatus::Completed)) => Response::HandoffCompleted,
                Ok(Ok(HandoffStatus::NotFound)) => Response::HandoffNotFound,
                Ok(Err(error)) => error.into(),
                Err(_) => Response::Unavailable,
            };
        }
        Action::Grant {
            publication,
            encoder,
            expires_at,
        } => {
            let source = Source {
                node: encoder.node,
                generation: encoder.generation,
                connection: encoder.connection,
                publication: encoder.publication.into(),
                track: encoder.track,
            };
            (
                publication.into(),
                Operation::Grant { source, expires_at },
                Some(source),
            )
        }
        Action::Advertise { delegation } => {
            let Ok((owner, source, grant)) = delegation.into_parts() else {
                return Response::InvalidRequest;
            };
            (
                source.publication,
                Operation::Advertise { owner, grant },
                None,
            )
        }
        Action::Select {
            publication,
            revision,
            delegation,
        } => {
            let delegated = match delegation {
                Some(image) => {
                    let Ok((_, source, grant)) = image.into_parts() else {
                        return Response::InvalidRequest;
                    };
                    Some((source, grant))
                }
                None => None,
            };
            (
                publication.into(),
                Operation::Select {
                    revision,
                    delegated,
                },
                None,
            )
        }
    };
    let (reply, result) = oneshot::channel();
    let command = StandbyControlCommand::new(StandbyRequest {
        resource,
        publication,
        operation,
        reply,
    });
    if sender
        .try_send(ControllerCommand::StandbyControl(command))
        .is_err()
    {
        return Response::Unavailable;
    }
    match (result.await, grant_source) {
        (Ok(Ok(Receipt::Granted { owner, grant })), Some(source)) => Response::Granted {
            delegation: Box::new(DelegationImage::new(*owner, source, grant)),
        },
        (Ok(Ok(Receipt::Queued)), None) => Response::Queued,
        (Ok(Err(error)), _) => error.into(),
        _ => Response::OutcomeUnknown,
    }
}

pub(super) async fn reply(
    bytes: &[u8],
    registry: &KeyRegistry,
    sender: &ControllerSender,
) -> serde_json::Result<Vec<u8>> {
    let response = timeout(COMMAND_TIMEOUT, dispatch(bytes, registry, sender))
        .await
        .unwrap_or(Response::OutcomeUnknown);
    serde_json::to_vec(&response)
}

/// The caller must hold this node's persistent membership directory lock.
pub(crate) fn bind(data_dir: &Path) -> anyhow::Result<StdListener> {
    let metadata = fs::symlink_metadata(data_dir)?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o022 == 0,
        "operator socket requires a data directory not writable by group or others"
    );
    let directory = data_dir.join("operator");
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let private = fs::symlink_metadata(&directory)?;
    anyhow::ensure!(
        private.is_dir()
            && private.permissions().mode() & 0o077 == 0
            && private.uid() == metadata.uid(),
        "operator directory must be private and owned by the data directory owner"
    );
    let socket = directory.join("ownership.sock");
    match fs::symlink_metadata(&socket) {
        Ok(entry) if entry.file_type().is_socket() && entry.uid() == private.uid() => {
            fs::remove_file(&socket)?;
        }
        Ok(_) => anyhow::bail!("operator socket path is occupied"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = StdListener::bind(&socket).context("binding local ownership socket")?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

pub(crate) async fn serve(
    listener: StdListener,
    registry: KeyRegistry,
    sender: ControllerSender,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let listener =
        UnixListener::from_std(listener).context("registering local ownership socket")?;
    let accept = async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut bytes = Vec::new();
            let mut body = match timeout(
                IO_TIMEOUT,
                (&mut stream)
                    .take((REQUEST_BYTES + 1) as u64)
                    .read_to_end(&mut bytes),
            )
            .await
            {
                Ok(Ok(_)) => reply(&bytes, &registry, &sender).await?,
                _ => serde_json::to_vec(&Response::InvalidRequest)?,
            };
            body.push(b'\n');
            let _ = timeout(IO_TIMEOUT, stream.write_all(&body)).await;
        }
    };
    pulsebeam_runtime::select! {
        result = accept => result,
        _ = shutdown.cancelled() => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::Shutdown,
        num::NonZeroUsize,
        os::unix::net::UnixStream,
    };

    use pulsebeam_core::auth::{
        DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_PUBLIC_KEY, DEVELOPMENT_PROJECT_ID,
        KeyRegistryEntry,
    };

    use super::*;

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn local_socket_reopens_and_rejects_unauthenticated_commands() -> anyhow::Result<()> {
        let directory = tempfile::tempdir_in("/tmp")?;
        drop(bind(directory.path())?);
        let listener = bind(directory.path())?;
        let socket = directory.path().join("operator/ownership.sock");
        assert_eq!(fs::metadata(&socket)?.permissions().mode() & 0o777, 0o600);
        let registry = KeyRegistry::new(
            *DEVELOPMENT_PROJECT_ID,
            vec![KeyRegistryEntry {
                key_id: *DEVELOPMENT_API_KEY_ID,
                public_key: *DEVELOPMENT_API_PUBLIC_KEY,
            }],
        )?;
        let (sender, mut receiver) = pulsebeam_queue::bounded(NonZeroUsize::MIN);
        let shutdown = CancellationToken::new();
        let client = pulsebeam_runtime::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut stream = UnixStream::connect(socket)?;
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            let request = serde_json::json!({
                "token": "not authenticated",
                "connection": ConnectionId::new(),
                "action": {
                    "operation": "select",
                    "revision": 1,
                    "publication": {"video": PublicationId::mint()},
                },
            });
            stream.write_all(&serde_json::to_vec(&request)?)?;
            stream.shutdown(Shutdown::Write)?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            assert_eq!(response, "{\"status\":\"unauthorized\"}\n");
            Ok(())
        });
        let (service, client) =
            pulsebeam_runtime::join!(serve(listener, registry, sender, shutdown.clone()), async {
                let result = client.await;
                shutdown.cancel();
                result
            },);
        service?;
        client??;
        assert!(receiver.try_recv().is_err());
        Ok(())
    }
}
