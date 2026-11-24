use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hyper::header::{CACHE_CONTROL, CONTENT_ENCODING};
use pulsebeam_core::{auth::VerifiedPrincipal, identity::RoomId};
use pulsebeam_queue::TrySendError;

use super::{
    ApiError, AppState, MAX_NATIVE_BODY_BYTES, NativeAdmissionRequest, NativeAdmissionResponse,
    ProblemResponse, authenticate_bearer, bearer_token, build_location, creation,
    ensure_json_content_type, native_message, parse_native_request,
};
use crate::control::controller;

const MAX_CAPABILITY_BODY_BYTES: usize = 1_024;
const MAX_QUIC_CAPABILITY_BODY_BYTES: usize = 2_048;
const QUIC_PROFILE: &str = "pulsebeam-rt-v1";

#[derive(serde::Serialize)]
struct QuicAdmissionResponse {
    owner: String,
    profile: &'static str,
    authority: QuicAuthority,
    valid_for_ms: u32,
    initial_dcid: String,
}

#[derive(serde::Serialize)]
struct QuicAuthority {
    host: String,
    port: u16,
}

fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<VerifiedPrincipal, ApiError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    authenticate_bearer(headers, &state.key_registry, now)
}

fn enqueue(state: &AppState, command: controller::AdmissionCommand) -> Result<(), ApiError> {
    state
        .controller
        .try_send(command.into())
        .map_err(|error| match error {
            TrySendError::Full(_) => ApiError::RateLimited,
            TrySendError::Closed(_) => ApiError::ServiceUnavailable,
        })
}

pub(super) async fn issue(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    let principal = authenticate(&state, request.headers())?;
    ensure_json_content_type(request.headers())?;
    creation::require_identity(request.headers())?;
    let body = to_bytes(request.into_body(), MAX_QUIC_CAPABILITY_BODY_BYTES)
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?;
    let ordinary_limit_exceeded = body.len() > MAX_CAPABILITY_BODY_BYTES;
    let invalid = || {
        if ordinary_limit_exceeded {
            ApiError::PayloadTooLarge
        } else {
            ApiError::BadRequest("invalid native admission JSON request".to_owned())
        }
    };
    if body.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(invalid());
    }
    let request: NativeAdmissionRequest = serde_json::from_slice(&body).map_err(|_| invalid())?;
    let room = request
        .room
        .as_deref()
        .map(RoomId::new)
        .transpose()
        .map_err(|_| invalid())?;
    let headers = [(CACHE_CONTROL, "no-store"), (CONTENT_ENCODING, "identity")];
    if let Some(profile) = request.profile {
        if profile != QUIC_PROFILE {
            return Err(invalid());
        }
        let continuation = match (request.owner, request.successor_receipt) {
            (None, None) if body.len() <= MAX_CAPABILITY_BODY_BYTES => None,
            (Some(owner), Some(receipt))
                if room.is_some() && owner.len() <= 256 && receipt.len() <= 1_024 =>
            {
                let proof = controller::ownership::Proof::parse(&owner).map_err(|_| invalid())?;
                let successor = URL_SAFE_NO_PAD.decode(receipt).map_err(|_| invalid())?;
                if successor.is_empty()
                    || successor.len() > pulsebeam_proto::state::MAX_SUCCESSOR_RECEIPT_BYTES
                {
                    return Err(invalid());
                }
                Some(controller::QuicContinuation { proof, successor })
            }
            _ => return Err(invalid()),
        };
        let (reply, received) = pulsebeam_runtime::sync::oneshot::channel();
        enqueue(
            &state,
            controller::AdmissionCommand::issue_quic(principal, room, continuation, reply),
        )?;
        let issuance = received.await.map_err(|_| ApiError::ServiceUnavailable)??;
        return Ok((
            headers,
            Json(QuicAdmissionResponse {
                owner: issuance.proof.encode(),
                profile: QUIC_PROFILE,
                authority: QuicAuthority {
                    host: issuance.host,
                    port: issuance.port,
                },
                valid_for_ms: issuance.valid_for_ms,
                initial_dcid: URL_SAFE_NO_PAD.encode(issuance.initial_dcid),
            }),
        )
            .into_response());
    }
    if ordinary_limit_exceeded {
        return Err(ApiError::PayloadTooLarge);
    }
    if request.owner.is_some() || request.successor_receipt.is_some() {
        return Err(invalid());
    }
    let (reply, received) = pulsebeam_runtime::sync::oneshot::channel();
    enqueue(
        &state,
        controller::AdmissionCommand::issue(principal, room, reply),
    )?;
    let proof = received.await.map_err(|_| ApiError::ServiceUnavailable)??;
    Ok((
        headers,
        Json(NativeAdmissionResponse {
            owner: proof.encode(),
        }),
    )
        .into_response())
}

pub(super) async fn create(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    let headers = request.headers().clone();
    let principal = authenticate(&state, &headers)?;
    ensure_json_content_type(&headers)?;
    let (proof, sequence) = creation::ownership_headers(&headers)?;
    creation::require_identity(&headers)?;
    let body = to_bytes(request.into_body(), MAX_NATIVE_BODY_BYTES)
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?;
    let request = parse_native_request(&body)?;
    let room = request
        .room
        .as_deref()
        .map(RoomId::new)
        .transpose()
        .map_err(|_| ApiError::BadRequest("invalid room external id".to_owned()))?;
    let authorization = principal
        .authorize_presence(room)
        .map_err(|_| controller::ControllerError::AuthorizationRejected)?;
    let (message, _) = native_message(&authorization, request.offer, SystemTime::now())?;
    let location = build_location(
        &headers,
        &state.api_config,
        &format!("/native/{}", message.state.connection_id),
    )?;
    let location = HeaderValue::from_str(&location).map_err(|_| ApiError::BadUrl)?;
    let (reply, received) = pulsebeam_runtime::sync::oneshot::channel();
    let binding =
        match controller::ownership::RequestBinding::for_native(&body, bearer_token(&headers)?) {
            Ok(binding) => binding,
            Err(error) => {
                return Ok(
                    ProblemResponse::from(controller::OwnedAdmissionError::from(error))
                        .into_response(),
                );
            }
        };
    enqueue(
        &state,
        controller::AdmissionCommand::create(
            message,
            proof,
            sequence,
            location,
            binding,
            request.successor_receipt,
            reply,
        ),
    )?;
    let delivery = match received.await.map_err(|_| ApiError::ServiceUnavailable)? {
        Ok(delivery) => delivery,
        Err(problem) => return Ok(problem.into_response()),
    };
    creation::into_response(delivery)
        .map_err(|_| ApiError::Unknown("invalid owned creation response".to_owned()))
}
