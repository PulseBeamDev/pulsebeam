use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
};
use hyper::header::{CACHE_CONTROL, CONTENT_ENCODING};
use pulsebeam_core::{auth::VerifiedPrincipal, identity::RoomExternalId};
use pulsebeam_queue::TrySendError;

use super::{
    ApiError, AppState, MAX_NATIVE_BODY_BYTES, NativeAdmissionRequest, NativeAdmissionResponse,
    ProblemResponse, authenticate_bearer, bearer_token, build_location, creation,
    ensure_json_content_type, native_message, parse_native_request,
};
use crate::control::controller;

const MAX_CAPABILITY_BODY_BYTES: usize = 1_024;

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
    let body = to_bytes(request.into_body(), MAX_CAPABILITY_BODY_BYTES)
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?;
    let invalid = || ApiError::BadRequest("an empty JSON object is required".to_owned());
    if body.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(invalid());
    }
    let _: NativeAdmissionRequest = serde_json::from_slice(&body).map_err(|_| invalid())?;
    let (reply, received) = pulsebeam_runtime::sync::oneshot::channel();
    enqueue(
        &state,
        controller::AdmissionCommand::issue(principal, reply),
    )?;
    let proof = received.await.map_err(|_| ApiError::ServiceUnavailable)??;
    Ok((
        [(CACHE_CONTROL, "no-store"), (CONTENT_ENCODING, "identity")],
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
    let room = RoomExternalId::new(&request.room)
        .map_err(|_| ApiError::BadRequest("invalid room external id".to_owned()))?;
    let authorization = principal
        .authorize_room(room)
        .map_err(|_| controller::ControllerError::AuthorizationRejected)?;
    let (message, _) = native_message(&authorization, request.offer, SystemTime::now())?;
    let location = build_location(
        &headers,
        &state.api_config,
        &format!("/native/{}", message.state.connection_id),
    )?;
    let location = HeaderValue::from_str(&location).map_err(|_| ApiError::BadUrl)?;
    let (reply, received) = pulsebeam_runtime::sync::oneshot::channel();
    let command = match controller::AdmissionCommand::create(
        message,
        proof,
        sequence,
        location,
        bearer_token(&headers)?,
        reply,
    ) {
        Ok(command) => command,
        Err(error) => return Ok(ProblemResponse::from(error).into_response()),
    };
    enqueue(&state, command)?;
    let delivery = match received.await.map_err(|_| ApiError::ServiceUnavailable)? {
        Ok(delivery) => delivery,
        Err(problem) => return Ok(problem.into_response()),
    };
    creation::into_response(delivery)
        .map_err(|_| ApiError::Unknown("invalid owned creation response".to_owned()))
}
