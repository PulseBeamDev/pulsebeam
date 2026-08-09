use std::collections::BTreeMap;

use axum::{
    Router,
    extract::{MatchedPath, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{patch, post},
};
use axum_extra::{TypedHeader, headers::ContentType};
use hyper::header::{ETAG, IF_MATCH, LOCATION};
use pulsebeam_runtime::mailbox::TrySendError;
use serde::Serialize;
use str0m::{change::SdpOffer, error::SdpError};
use tokio::time::Instant;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use crate::{
    control::api_json,
    control::auth,
    control::controller::{self},
    entity::ConnectionId,
};
use crate::{
    control::controller::{ControllerHandle, ParticipantState},
    entity::{ConnectionEpoch, ExternalRoomId, IdValidationError, ParticipantId, RoomId},
};
use pulsebeam_runtime::rand::os_rng;
pub enum HeaderExt {
    ParticipantId,
}

impl HeaderExt {
    pub fn as_str(&self) -> &str {
        match self {
            Self::ParticipantId => "pb-participant-id",
        }
    }
}

/// Response headers for participant creation
#[derive(Debug, Serialize, ToSchema)]
pub struct ParticipantResponseHeaders {
    /// URL of the created participant resource
    #[serde(rename = "Location")]
    pub location: String,

    #[serde(rename = "ETag")]
    pub etag: ConnectionId,
}

impl ParticipantResponseHeaders {
    pub fn to_header_map(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(LOCATION, self.location.parse().unwrap());
        headers.insert(ETAG, self.etag.as_str().parse().unwrap());
        headers
    }
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) controller: ControllerHandle,
    pub(crate) api_config: ApiConfig,
}

/// Above any realistic SDP, below axum's 2 MiB default.
pub const DEFAULT_MAX_SIGNALING_BODY: usize = 256 * 1024;

/// Configuration shared across handlers
#[derive(Clone)]
pub struct ApiConfig {
    pub base_path: String,    // e.g. "/api/v1"
    pub default_host: String, // fallback if no Host header, e.g. "localhost:7070"
    /// `None` disables the JSON surface entirely; the SDP surface is unaffected.
    pub auth: Option<std::sync::Arc<auth::AuthConfig>>,
    /// Also require a bearer token on the legacy `application/sdp` endpoints.
    pub require_auth: bool,
    pub max_body_bytes: usize,
}

impl ApiConfig {
    pub fn new(base_path: impl Into<String>, default_host: impl Into<String>) -> Self {
        Self {
            base_path: base_path.into(),
            default_host: default_host.into(),
            auth: None,
            require_auth: false,
            max_body_bytes: DEFAULT_MAX_SIGNALING_BODY,
        }
    }
}

/// Error type for api operations
#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("invalid entity id format: {0}")]
    IdValidation(#[from] IdValidationError),
    #[error("sdp offer is invalid: {0}")]
    OfferInvalid(#[from] SdpError),
    #[error("join failed: {0}")]
    JoinError(#[from] controller::ControllerError),
    #[error("too many requests, please try again later.")]
    RateLimited,
    #[error("server is busy, please try again later")]
    ServiceUnavailable,
    #[error("failed to construct response URL")]
    BadUrl,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(#[from] auth::AuthError),
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    #[error("request body is not valid json: {0}")]
    InvalidJson(String),
    #[error("unsupported media type")]
    UnsupportedMediaType,
    #[error("{0}")]
    Unknown(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::IdValidation(_)
            | ApiError::OfferInvalid(_)
            | ApiError::JoinError(controller::ControllerError::OfferRejected(_))
            | ApiError::InvalidJson(_)
            | ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized(e) if e.is_forbidden() => StatusCode::FORBIDDEN,
            ApiError::Unauthorized(e) if e.is_unavailable() => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ApiError::JoinError(controller::ControllerError::ServiceUnavailable)
            | ApiError::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            ApiError::JoinError(controller::ControllerError::ParticipantNotFound) => {
                StatusCode::NOT_FOUND
            }
            ApiError::JoinError(controller::ControllerError::ConnectionMismatch) => {
                StatusCode::PRECONDITION_FAILED
            }
            ApiError::JoinError(controller::ControllerError::Unknown(_))
            | ApiError::JoinError(controller::ControllerError::IOError(_))
            | ApiError::BadUrl
            | ApiError::Unknown(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Stable machine-readable code for the JSON envelope.
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::IdValidation(_) => "invalid_id",
            ApiError::OfferInvalid(_) => "invalid_sdp",
            ApiError::InvalidJson(_) => "invalid_json",
            ApiError::BadRequest(_) => "bad_request",
            ApiError::Unauthorized(e) => e.code(),
            ApiError::Forbidden(code) => code,
            ApiError::UnsupportedMediaType => "unsupported_media_type",
            ApiError::RateLimited => "rate_limited",
            ApiError::ServiceUnavailable => "service_unavailable",
            ApiError::BadUrl | ApiError::Unknown(_) => "internal",
            ApiError::JoinError(e) => match e {
                controller::ControllerError::OfferRejected(_) => "invalid_sdp",
                controller::ControllerError::ServiceUnavailable => "service_unavailable",
                controller::ControllerError::ParticipantNotFound => "participant_not_found",
                controller::ControllerError::ConnectionMismatch => "connection_mismatch",
                controller::ControllerError::IOError(_)
                | controller::ControllerError::Unknown(_) => "internal",
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.status(), self.to_string()).into_response()
    }
}

/// Build an absolute URL for Location header
fn build_location(
    headers: &HeaderMap,
    cfg: &ApiConfig,
    path: &str,
    state: &ParticipantState,
) -> Result<String, ApiError> {
    build_location_for(headers, cfg, path, state.manual_sub)
}

pub(crate) fn build_location_for(
    headers: &HeaderMap,
    cfg: &ApiConfig,
    path: &str,
    manual_sub: bool,
) -> Result<String, ApiError> {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");

    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&cfg.default_host);

    // TODO: Can these keys be strongly typed?
    let mut params = BTreeMap::new();
    if manual_sub {
        params.insert("manual_sub".to_string(), "true".to_string());
    }

    // url::form_urlencoded uses the BTreeMap iterator, maintaining alphabetical order
    let query_string = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter())
        .finish();

    let url = format!(
        "{}://{}{}{}?{}",
        scheme, host, cfg.base_path, path, query_string
    );
    url.parse::<Uri>().map_err(|_| ApiError::BadUrl)?;

    Ok(url)
}

#[derive(serde::Deserialize)]
pub struct CreateParticipantQuery {
    #[serde(default)]
    pub manual_sub: bool,
}

/// Which participant a join is for: a fresh one, or an existing identity being re-established.
pub(crate) enum JoinKind {
    Create,
    Reconnect {
        participant_id: ParticipantId,
        old_connection_id: Option<ConnectionId>,
        /// Lower bound for the new connection's generation, from a resume token when the registry
        /// may no longer remember this participant.
        epoch: ConnectionEpoch,
    },
}

pub(crate) struct JoinOutcome {
    pub state: ParticipantState,
    pub answer: str0m::change::SdpAnswer,
    pub location: String,
}

fn to_api_error(e: TrySendError<controller::ControllerCommand>) -> ApiError {
    match e {
        TrySendError::Full(_) => ApiError::RateLimited,
        TrySendError::Closed(_) => ApiError::ServiceUnavailable,
    }
}

/// The path every representation shares: mint ids, hand the offer to the controller, await the
/// answer, and build the resource URL. Both the SDP handlers and the JSON handlers go through here
/// so the two can never drift on what reaches the controller.
pub(crate) async fn join_core(
    s: &AppState,
    headers: &HeaderMap,
    kind: JoinKind,
    external_room_id: &ExternalRoomId,
    manual_sub: bool,
    offer: SdpOffer,
) -> Result<JoinOutcome, ApiError> {
    let room_id = RoomId::from_external(external_room_id);

    let (participant_id, old_connection_id, epoch) = match kind {
        JoinKind::Create => (
            ParticipantId::new(&mut os_rng()),
            None,
            ConnectionEpoch::ZERO,
        ),
        JoinKind::Reconnect {
            participant_id,
            old_connection_id,
            epoch,
        } => (participant_id, old_connection_id, epoch),
    };
    // A capability token, so it is derived from fresh OS entropy on every join.
    let connection_id = ConnectionId::new(&mut os_rng());

    let state = ParticipantState {
        manual_sub,
        room_id,
        participant_id,
        connection_id,
        old_connection_id,
        epoch,
    };

    let answer = if old_connection_id.is_some() {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let msg = controller::PatchParticipant {
            offer,
            state: state.clone(),
        };
        s.controller
            .try_send((msg, reply_tx).into())
            .map_err(to_api_error)?;
        reply_rx
            .await
            .map_err(|_| controller::ControllerError::ServiceUnavailable)??
            .answer
    } else {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let msg = controller::CreateParticipant {
            offer,
            state: state.clone(),
        };
        s.controller
            .try_send((msg, reply_tx).into())
            .map_err(to_api_error)?;
        reply_rx
            .await
            .map_err(|_| controller::ControllerError::ServiceUnavailable)??
            .answer
    };

    let path = format!(
        "/rooms/{}/participants/{}",
        external_room_id, &participant_id
    );
    let location = build_location(headers, &s.api_config, &path, &state)?;

    debug_assert_eq!(state.participant_id, participant_id);
    debug_assert!(location.starts_with("http"));
    debug_assert!(
        state.old_connection_id != Some(state.connection_id),
        "a reconnect must mint a connection id distinct from the one it replaces"
    );

    Ok(JoinOutcome {
        state,
        answer,
        location,
    })
}

/// Create a new participant in a room
///
/// Creates a new participant by processing a WebRTC offer and returning an answer.
/// The participant ID is generated and returned in both the Location header and
/// the pb-participant-id header.
#[utoipa::path(
    post,
    path = "/rooms/{external_room_id}/participants",
    request_body(content = String, description = "WebRTC SDP offer", content_type = "application/sdp"),
    params(
        ("external_room_id" = String, Path, description = "External room identifier")
    ),
    responses(
        (status = 201, description = "Participant created successfully", body = String,
            headers(
                ("Location" = String, description = "URL of the created participant resource"),
                ("pb-participant-id" = String, description = "participant ID")
            ),
            content_type = "application/sdp"
        ),
        (status = 400, description = "Invalid or rejected offer", body = String, content_type = "text/plain"),
        (status = 500, description = "Internal server error", body = String, content_type = "text/plain"),
        (status = 503, description = "Service unavailable", body = String, content_type = "text/plain")
    ),
    tag = "participants"
)]
#[axum::debug_handler]
async fn create_participant(
    Path(external_room_id): Path<ExternalRoomId>,
    Query(query): Query<CreateParticipantQuery>,
    State(s): State<AppState>,
    TypedHeader(_content_type): TypedHeader<ContentType>,
    headers: HeaderMap,
    raw_offer: String,
) -> Result<impl IntoResponse, ApiError> {
    let offer = SdpOffer::from_sdp_string(&raw_offer)?;

    let outcome = join_core(
        &s,
        &headers,
        JoinKind::Create,
        &external_room_id,
        query.manual_sub,
        offer,
    )
    .await?;

    let response_headers = ParticipantResponseHeaders {
        location: outcome.location,
        etag: outcome.state.connection_id,
    };

    Ok((
        StatusCode::CREATED,
        response_headers.to_header_map(),
        outcome.answer.to_sdp_string(),
    ))
}

/// Delete a participant from a room
///
/// Removes a participant from the specified room. This will clean up all
/// resources associated with the participant.
#[utoipa::path(
    delete,
    path = "/rooms/{external_room_id}/participants/{participant_id}",
    params(
        ("external_room_id" = String, Path, description = "External room identifier"),
        ("participant_id" = String, Path, description = "Participant identifier")
    ),
    responses(
        (status = 204, description = "Participant deleted successfully"),
        (status = 500, description = "Internal server error", body = String, content_type = "text/plain")
    ),
    tag = "participants"
)]
#[axum::debug_handler]
async fn delete_participant(
    Path((external_room_id, participant_id)): Path<(ExternalRoomId, ParticipantId)>,
    State(s): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    let room_id = RoomId::from_external(&external_room_id);
    s.controller
        .try_send(
            controller::DeleteParticipant {
                room_id,
                participant_id,
                connection_id: None,
                reply: None,
            }
            .into(),
        )
        .map_err(|e| match e {
            TrySendError::Full(_) => ApiError::RateLimited,
            TrySendError::Closed(_) => ApiError::ServiceUnavailable,
        })?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
pub struct PatchParticipantQuery {
    #[serde(default)]
    pub manual_sub: bool,
}

/// Reconnect a participant to a room
///
/// Allows a participant to reconnect to a room with new WebRTC offer,
/// using their existing track IDs and providing authentication signature.
#[utoipa::path(
    patch,
    path = "/rooms/{external_room_id}/participants/{participant_id}",
    request_body(content = String, description = "WebRTC SDP offer for reconnection", content_type = "application/sdp"),
    params(
        ("external_room_id" = String, Path, description = "External room identifier"),
        ("participant_id" = String, Path, description = "Participant identifier")
    ),
    responses(
        (status = 200, description = "Reconnection successful", body = String,
            content_type = "application/sdp"
        ),
        (status = 400, description = "Invalid request or missing headers", body = String, content_type = "text/plain"),
        (status = 401, description = "Invalid signature", body = String, content_type = "text/plain"),
        (status = 412, description = "Precondition failed - ETag mismatch", body = String, content_type = "text/plain"),
        (status = 500, description = "Internal server error", body = String, content_type = "text/plain"),
        (status = 503, description = "Service unavailable", body = String, content_type = "text/plain")
    ),
    tag = "participants"
)]
#[axum::debug_handler]
async fn patch_participant(
    Path((external_room_id, participant_id)): Path<(ExternalRoomId, ParticipantId)>,
    Query(query): Query<PatchParticipantQuery>,
    State(s): State<AppState>,
    TypedHeader(_content_type): TypedHeader<ContentType>,
    headers: HeaderMap,
    raw_offer: String,
) -> Result<impl IntoResponse, ApiError> {
    let offer = SdpOffer::from_sdp_string(&raw_offer)?;

    let old_connection_id: ConnectionId = headers
        .get(IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"'))
        .ok_or(ApiError::BadRequest("If-Match header required".into()))?
        .try_into()?;

    let outcome = join_core(
        &s,
        &headers,
        JoinKind::Reconnect {
            participant_id,
            old_connection_id: Some(old_connection_id),
            epoch: ConnectionEpoch::ZERO,
        },
        &external_room_id,
        query.manual_sub,
        offer,
    )
    .await?;

    let response_headers = ParticipantResponseHeaders {
        location: outcome.location,
        etag: outcome.state.connection_id,
    };

    Ok((
        StatusCode::OK,
        response_headers.to_header_map(),
        outcome.answer.to_sdp_string(),
    ))
}

/// `Content-Type` selects the representation; anything that is not JSON falls through to the
/// legacy SDP handler untouched, including the 400 for a missing `Content-Type`.
async fn create_participant_dispatch(
    Path(external_room_id): Path<ExternalRoomId>,
    Query(query): Query<CreateParticipantQuery>,
    State(s): State<AppState>,
    content_type: Option<TypedHeader<ContentType>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    debug_assert!(body.len() <= s.api_config.max_body_bytes);

    if api_json::is_json(&headers) {
        return match api_json::join(s, external_room_id, headers, body).await {
            Ok(response) => response,
            Err(e) => e.into_response(),
        };
    }

    // Preserved verbatim: the legacy handler requires a Content-Type but ignores its value.
    if content_type.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "Header of type `content-type` was missing",
        )
            .into_response();
    }
    let Ok(raw_offer) = String::from_utf8(body.to_vec()) else {
        return (
            StatusCode::BAD_REQUEST,
            "Request body didn't contain valid UTF-8",
        )
            .into_response();
    };

    if let Err(e) = require_auth_if_configured(&s, &headers, &external_room_id) {
        return e.into_response();
    }

    create_participant(
        Path(external_room_id),
        Query(query),
        State(s),
        content_type.unwrap(),
        headers,
        raw_offer,
    )
    .await
    .into_response()
}

async fn patch_participant_dispatch(
    Path((external_room_id, participant_id)): Path<(ExternalRoomId, ParticipantId)>,
    Query(query): Query<PatchParticipantQuery>,
    State(s): State<AppState>,
    content_type: Option<TypedHeader<ContentType>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // PATCH has no JSON meaning: renegotiation is not part of this surface.
    if api_json::is_json(&headers) {
        return api_json::JsonApiError(ApiError::UnsupportedMediaType).into_response();
    }
    if content_type.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "Header of type `content-type` was missing",
        )
            .into_response();
    }
    let Ok(raw_offer) = String::from_utf8(body.to_vec()) else {
        return (
            StatusCode::BAD_REQUEST,
            "Request body didn't contain valid UTF-8",
        )
            .into_response();
    };

    if let Err(e) = require_auth_if_configured(&s, &headers, &external_room_id) {
        return e.into_response();
    }

    patch_participant(
        Path((external_room_id, participant_id)),
        Query(query),
        State(s),
        content_type.unwrap(),
        headers,
        raw_offer,
    )
    .await
    .into_response()
}

async fn delete_participant_dispatch(
    Path((external_room_id, participant_id)): Path<(ExternalRoomId, ParticipantId)>,
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Response {
    // Checked before the representation is chosen. The representation is selected from headers
    // the caller controls, so gating inside one branch would let a caller opt out of
    // authentication simply by sending no credential.
    if let Err(e) = require_auth_if_configured(&s, &headers, &external_room_id) {
        return e.into_response();
    }

    if api_json::wants_json_delete(&headers) {
        return match api_json::leave(s, external_room_id, participant_id, headers).await {
            Ok(response) => response,
            Err(e) => e.into_response(),
        };
    }

    delete_participant(Path((external_room_id, participant_id)), State(s))
        .await
        .into_response()
}

/// Bearer auth on the legacy SDP surface, off unless the operator opts in.
fn require_auth_if_configured(
    s: &AppState,
    headers: &HeaderMap,
    room: &ExternalRoomId,
) -> Result<(), ApiError> {
    if !s.api_config.require_auth {
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    api_json::authorize(&s.api_config, headers, room, now)?;
    Ok(())
}

/// Build OpenAPI spec with dynamic server configuration
fn build_openapi(base_path: &str) -> utoipa::openapi::OpenApi {
    use utoipa::openapi::{ContactBuilder, InfoBuilder, OpenApi as OpenApiSpec, ServerBuilder};

    let info = InfoBuilder::new()
        .title("PulseBeam API")
        .version("1.0.0")
        .description(Some("API for managing PulseBeam room & participants"))
        .contact(Some(
            ContactBuilder::new()
                .name(Some("API Support"))
                .email(Some("lukas@pulsebeam.dev"))
                .build(),
        ))
        .build();

    let mut openapi = OpenApiSpec::new(info, utoipa::openapi::path::Paths::new());

    // Add server with the configured base path
    openapi.servers = Some(vec![
        ServerBuilder::new()
            .url(base_path)
            .description(Some("API Server"))
            .build(),
    ]);

    // Merge in the generated paths
    let generated = ApiDoc::openapi();
    openapi.paths = generated.paths;
    openapi.components = generated.components;
    openapi.tags = generated.tags;

    openapi
}

/// OpenAPI documentation structure (without servers, we'll set those dynamically)
#[derive(OpenApi)]
#[openapi(
    paths(
        create_participant,
        patch_participant,
        delete_participant,
        api_json::join,
        api_json::resume,
    ),
    components(
        schemas(
            ParticipantResponseHeaders,
            api_json::JoinRequest,
            api_json::ResumeRequest,
            api_json::SessionResponse,
            api_json::ErrorResponse,
        )
    ),
    tags(
        (name = "participants", description = "Participant management endpoints (application/sdp)"),
        (name = "participants-json", description = "Participant management endpoints (application/json)"),
    )
)]
struct ApiDoc;

/// Router setup with OpenAPI documentation
pub fn router(controller: controller::ControllerHandle, cfg: ApiConfig) -> Router {
    let openapi = build_openapi(&cfg.base_path);

    let max_body = cfg.max_body_bytes;
    let api = Router::new()
        .route(
            "/rooms/{external_room_id}/participants",
            post(create_participant_dispatch),
        )
        .route(
            "/rooms/{external_room_id}/participants/{participant_id}",
            patch(patch_participant_dispatch)
                .put(api_json::resume)
                .delete(delete_participant_dispatch),
        )
        .layer(axum::extract::DefaultBodyLimit::max(max_body))
        .layer(middleware::from_fn(track_route_duration));

    Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", openapi))
        .nest(&cfg.base_path, api)
        .with_state(AppState {
            controller,
            api_config: cfg,
        })
}

async fn track_route_duration(req: Request<axum::body::Body>, next: Next) -> Response {
    let start = Instant::now();

    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unknown_route".to_string());

    let method = req.method().to_string();
    let response = next.run(req).await;
    let duration = start.elapsed().as_secs_f64();

    metrics::histogram!("http_request_duration_seconds", "path" => path, "method" => method)
        .record(duration);
    response
}

/// Golden coverage for the `application/sdp` surface.
///
/// These assert exact bytes, not shapes. Existing clients depend on this surface verbatim, so any
/// difference -- a header that stops being emitted, a status that shifts, a body that re-serializes
/// differently -- must fail here rather than in the field.
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use hyper::header::CONTENT_TYPE;
    use pulsebeam_runtime::mailbox;
    use str0m::change::SdpAnswer;
    use tower::ServiceExt;

    const ROOM: &str = "standup";
    const PARTICIPANT: &str = "pa_8ZQ4W2P0H3RJ6VC1TKXE5N7BMD";

    fn offer_sdp() -> String {
        pulsebeam_testdata::RAW_CHROME_SDP.to_string()
    }

    fn answer_sdp() -> SdpAnswer {
        // str0m has no answer fixture; an offer parsed as an answer is structurally identical and
        // all these tests need is a stable, non-empty body the handler will serialize.
        SdpAnswer::from_sdp_string(pulsebeam_testdata::RAW_CHROME_SDP).unwrap()
    }

    /// How the stub controller should respond, so error paths are reachable without a real actor.
    enum Stub {
        Answer,
        Reject,
        /// Dropped immediately, so the handler sees `Closed`.
        Closed,
        /// Receiver held but never drained, and the single slot pre-filled, so every handler
        /// `try_send` sees `Full`. Deterministic: no spawning, no waiting for a queue to fill.
        Saturated,
    }

    struct Harness {
        router: Router,
        commands: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
        /// Kept alive so a `Saturated` mailbox reads as full rather than closed.
        _rx: Option<mailbox::Receiver<controller::ControllerCommand>>,
    }

    fn harness(stub: Stub) -> Harness {
        let (tx, mut rx) = mailbox::new::<controller::ControllerCommand>(1);
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut retained = None;

        match stub {
            Stub::Closed => drop(rx),
            Stub::Saturated => {
                tx.try_send(
                    controller::DeleteParticipant {
                        room_id: RoomId::from_external(&ExternalRoomId::new(ROOM).unwrap()),
                        participant_id: PARTICIPANT.parse().unwrap(),
                        connection_id: None,
                        reply: None,
                    }
                    .into(),
                )
                .expect("the first message fits");
                retained = Some(rx);
            }
            Stub::Answer | Stub::Reject => {
                let seen = commands.clone();
                let reject = matches!(stub, Stub::Reject);
                tokio::spawn(async move {
                    while let Some(cmd) = rx.recv().await {
                        match cmd {
                            controller::ControllerCommand::CreateParticipant(_, reply) => {
                                seen.lock().unwrap().push("create");
                                let _ = reply.send(if reject {
                                    Err(controller::ControllerError::ServiceUnavailable)
                                } else {
                                    Ok(controller::CreateParticipantReply {
                                        answer: answer_sdp(),
                                    })
                                });
                            }
                            controller::ControllerCommand::PatchParticipant(_, reply) => {
                                seen.lock().unwrap().push("patch");
                                let _ = reply.send(if reject {
                                    Err(controller::ControllerError::ServiceUnavailable)
                                } else {
                                    Ok(controller::PatchParticipantReply {
                                        answer: answer_sdp(),
                                    })
                                });
                            }
                            controller::ControllerCommand::DeleteParticipant(m) => {
                                seen.lock().unwrap().push("delete");
                                if let Some(reply) = m.reply {
                                    let _ = reply.send(Ok(()));
                                }
                            }
                            controller::ControllerCommand::ResumeParticipant(_, reply) => {
                                seen.lock().unwrap().push("resume");
                                let _ = reply.send(if reject {
                                    Err(controller::ControllerError::ServiceUnavailable)
                                } else {
                                    Ok(controller::ResumeParticipantReply {
                                        answer: answer_sdp(),
                                        existed: false,
                                        epoch: crate::entity::ConnectionEpoch::ZERO,
                                    })
                                });
                            }
                        }
                    }
                });
            }
        }

        Harness {
            router: router(tx, ApiConfig::new("/api/v1", "sfu.test")),
            commands,
            _rx: retained,
        }
    }

    struct Captured {
        status: StatusCode,
        headers: HeaderMap,
        body: String,
    }

    impl Captured {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(name).and_then(|v| v.to_str().ok())
        }
    }

    async fn send(h: &Harness, req: Request<Body>) -> Captured {
        let response = h.router.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Captured {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    fn post(body: &str, content_type: Option<&str>) -> Request<Body> {
        let mut req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/rooms/{ROOM}/participants"))
            .header("host", "sfu.test");
        if let Some(ct) = content_type {
            req = req.header(CONTENT_TYPE, ct);
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    fn patch(body: &str, if_match: Option<&str>) -> Request<Body> {
        let mut req = Request::builder()
            .method("PATCH")
            .uri(format!("/api/v1/rooms/{ROOM}/participants/{PARTICIPANT}"))
            .header("host", "sfu.test")
            .header(CONTENT_TYPE, "application/sdp");
        if let Some(etag) = if_match {
            req = req.header(IF_MATCH, etag);
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn post_returns_201_with_location_etag_and_the_answer_verbatim() {
        let h = harness(Stub::Answer);
        let res = send(&h, post(&offer_sdp(), Some("application/sdp"))).await;

        assert_eq!(res.status, StatusCode::CREATED);
        assert_eq!(res.body, answer_sdp().to_sdp_string());

        let location = res
            .header("location")
            .expect("Location is part of the contract");
        assert!(
            location.starts_with("http://sfu.test/api/v1/rooms/standup/participants/pa_"),
            "unexpected Location: {location}"
        );
        // No manual_sub means an empty query string, trailing '?' included.
        assert!(location.ends_with('?'), "unexpected Location: {location}");

        let etag = res.header("etag").expect("ETag is part of the contract");
        assert!(etag.starts_with("c_"), "unexpected ETag: {etag}");
        // Emitted unquoted today. Clients parse it as a ConnectionId, so quoting would break them.
        assert!(!etag.starts_with('"'), "ETag must stay unquoted: {etag}");

        assert_eq!(*h.commands.lock().unwrap(), vec!["create"]);
    }

    #[tokio::test]
    async fn post_carries_manual_sub_into_the_location() {
        let h = harness(Stub::Answer);
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/rooms/{ROOM}/participants?manual_sub=true"))
            .header("host", "sfu.test")
            .header(CONTENT_TYPE, "application/sdp")
            .body(Body::from(offer_sdp()))
            .unwrap();
        let res = send(&h, req).await;

        assert_eq!(res.status, StatusCode::CREATED);
        assert!(
            res.header("location")
                .unwrap()
                .ends_with("?manual_sub=true")
        );
    }

    #[tokio::test]
    async fn a_hostile_forwarded_host_cannot_kill_the_node() {
        // Location is built from caller-supplied headers and then parsed into a HeaderValue.
        // Under `panic = "abort"` a panic here is a remote node kill, so every shape that can
        // reach the parser must produce a response instead.
        let h = harness(Stub::Answer);
        let long = "a".repeat(9000);
        // Control characters are rejected by the header parser before reaching us, so the
        // interesting cases are values that are valid headers but hostile inside a URL.
        for host in ["host with spaces", "@@@", "[::1", "a/../b", "x?y#z", &long] {
            let req = Request::builder()
                .method("POST")
                .uri(format!("/api/v1/rooms/{ROOM}/participants"))
                .header("host", "sfu.test")
                .header(CONTENT_TYPE, "application/sdp")
                .header("x-forwarded-host", host)
                .body(Body::from(offer_sdp()))
                .unwrap();
            let res = send(&h, req).await;
            assert!(
                res.status.is_success()
                    || res.status.is_client_error()
                    || res.status.is_server_error(),
                "{host:?} produced no response"
            );
        }
    }

    #[tokio::test]
    async fn post_honours_forwarded_scheme_and_host() {
        let h = harness(Stub::Answer);
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/rooms/{ROOM}/participants"))
            .header("host", "internal:7070")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "edge.example.com")
            .header(CONTENT_TYPE, "application/sdp")
            .body(Body::from(offer_sdp()))
            .unwrap();
        let res = send(&h, req).await;

        assert!(
            res.header("location")
                .unwrap()
                .starts_with("https://edge.example.com/api/v1/"),
            "proxy headers must win over Host"
        );
    }

    #[tokio::test]
    async fn a_missing_content_type_is_rejected_before_the_controller() {
        let h = harness(Stub::Answer);
        let res = send(&h, post(&offer_sdp(), None)).await;

        assert_eq!(res.status, StatusCode::BAD_REQUEST);
        assert!(
            h.commands.lock().unwrap().is_empty(),
            "a rejected request must never reach the controller"
        );
    }

    #[tokio::test]
    async fn any_content_type_is_accepted_on_the_legacy_path() {
        // The handler extracts Content-Type but ignores its value. Documented here because the
        // JSON dispatcher must preserve exactly this behaviour for non-JSON types.
        let h = harness(Stub::Answer);
        let res = send(&h, post(&offer_sdp(), Some("text/plain"))).await;
        assert_eq!(res.status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn an_unparseable_offer_is_a_400_with_a_text_plain_body() {
        let h = harness(Stub::Answer);
        let res = send(&h, post("this is not sdp", Some("application/sdp"))).await;

        assert_eq!(res.status, StatusCode::BAD_REQUEST);
        assert!(
            res.body.starts_with("sdp offer is invalid:"),
            "body: {}",
            res.body
        );
        assert!(h.commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_invalid_room_id_is_a_400() {
        let h = harness(Stub::Answer);
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/rooms/not%20a%20room/participants")
            .header("host", "sfu.test")
            .header(CONTENT_TYPE, "application/sdp")
            .body(Body::from(offer_sdp()))
            .unwrap();
        let res = send(&h, req).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_rejected_offer_surfaces_the_controller_status() {
        let h = harness(Stub::Reject);
        let res = send(&h, post(&offer_sdp(), Some("application/sdp"))).await;
        assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_closed_controller_is_a_503() {
        let h = harness(Stub::Closed);
        let res = send(&h, post(&offer_sdp(), Some("application/sdp"))).await;
        assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_saturated_controller_is_a_429_on_every_verb() {
        // Backpressure is the only rate limiting the server has; each verb must surface it.
        for req in [
            post(&offer_sdp(), Some("application/sdp")),
            patch(&offer_sdp(), Some("c_R5T9K2ND7QW0J4XVA8ZP1MHC3B")),
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/rooms/{ROOM}/participants/{PARTICIPANT}"))
                .header("host", "sfu.test")
                .body(Body::empty())
                .unwrap(),
        ] {
            let h = harness(Stub::Saturated);
            let method = req.method().clone();
            let res = send(&h, req).await;
            assert_eq!(
                res.status,
                StatusCode::TOO_MANY_REQUESTS,
                "{method} did not surface backpressure"
            );
        }
    }

    #[tokio::test]
    async fn patch_requires_if_match_and_returns_200_with_a_rotated_etag() {
        let h = harness(Stub::Answer);
        let etag = "c_R5T9K2ND7QW0J4XVA8ZP1MHC3B";
        let res = send(&h, patch(&offer_sdp(), Some(etag))).await;

        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(res.body, answer_sdp().to_sdp_string());
        let rotated = res.header("etag").unwrap();
        assert!(rotated.starts_with("c_"));
        assert_ne!(rotated, etag, "PATCH mints a fresh connection id");
        assert_eq!(*h.commands.lock().unwrap(), vec!["patch"]);
    }

    #[tokio::test]
    async fn patch_accepts_a_quoted_if_match() {
        let h = harness(Stub::Answer);
        let res = send(
            &h,
            patch(&offer_sdp(), Some("\"c_R5T9K2ND7QW0J4XVA8ZP1MHC3B\"")),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn patch_without_if_match_is_a_400() {
        let h = harness(Stub::Answer);
        let res = send(&h, patch(&offer_sdp(), None)).await;

        assert_eq!(res.status, StatusCode::BAD_REQUEST);
        assert_eq!(res.body, "bad request: If-Match header required");
        assert!(h.commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn patch_with_a_malformed_if_match_is_a_400() {
        let h = harness(Stub::Answer);
        let res = send(&h, patch(&offer_sdp(), Some("not-a-connection-id"))).await;

        assert_eq!(res.status, StatusCode::BAD_REQUEST);
        assert!(h.commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_is_unconditional_and_returns_204_with_no_body() {
        // Today DELETE takes no ETag and does not check existence. Captured so the JSON path's
        // stricter behaviour is visibly a *new* surface rather than a change to this one.
        let h = harness(Stub::Answer);
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/rooms/{ROOM}/participants/{PARTICIPANT}"))
            .header("host", "sfu.test")
            .body(Body::empty())
            .unwrap();
        let res = send(&h, req).await;

        assert_eq!(res.status, StatusCode::NO_CONTENT);
        assert_eq!(res.body, "");
    }

    #[tokio::test]
    async fn errors_are_plain_text_not_json() {
        let h = harness(Stub::Answer);
        let res = send(&h, patch(&offer_sdp(), None)).await;
        let content_type = res.header("content-type").unwrap_or_default();
        assert!(
            content_type.starts_with("text/plain"),
            "legacy errors must stay text/plain, got {content_type}"
        );
        assert!(serde_json::from_str::<serde_json::Value>(&res.body).is_err());
    }

    #[tokio::test]
    async fn unknown_routes_and_methods_are_not_served() {
        let h = harness(Stub::Answer);
        // PUT is deliberately absent from this list: it is the resume verb, added by the JSON
        // surface. Everything else stays unserved.
        for (method, uri) in [
            ("GET", format!("/api/v1/rooms/{ROOM}/participants")),
            ("HEAD", format!("/api/v1/rooms/{ROOM}/participants")),
            ("POST", format!("/api/v1/rooms/{ROOM}")),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(&uri)
                .header("host", "sfu.test")
                .body(Body::empty())
                .unwrap();
            let res = send(&h, req).await;
            assert!(
                res.status == StatusCode::NOT_FOUND || res.status == StatusCode::METHOD_NOT_ALLOWED,
                "{method} {uri} unexpectedly served: {}",
                res.status
            );
        }
    }
}

#[cfg(test)]
mod openapi_tests {
    use super::*;

    #[test]
    fn the_openapi_document_describes_both_representations() {
        let spec = build_openapi("/api/v1");
        let json = serde_json::to_value(&spec).unwrap();
        let paths = &json["paths"];

        let participant = "/rooms/{external_room_id}/participants/{participant_id}";
        assert!(paths["/rooms/{external_room_id}/participants"]["post"].is_object());
        assert!(paths[participant]["patch"].is_object());
        assert!(paths[participant]["delete"].is_object());
        // The resume verb must be documented, not just routed.
        assert!(
            paths[participant]["put"].is_object(),
            "PUT is the resume verb and belongs in the spec"
        );

        // The 401/412 responses were documented long before anything could produce them; now that
        // they are real, they must still be described.
        let patch_responses = &paths[participant]["patch"]["responses"];
        assert!(patch_responses["401"].is_object());
        assert!(patch_responses["412"].is_object());

        let put_responses = &paths[participant]["put"]["responses"];
        assert!(
            put_responses["200"].is_object(),
            "replaced a live participant"
        );
        assert!(
            put_responses["201"].is_object(),
            "reconstructed one that was gone"
        );
    }

    #[test]
    fn the_json_schemas_are_published() {
        let spec = build_openapi("/api/v1");
        let json = serde_json::to_value(&spec).unwrap();
        let schemas = &json["components"]["schemas"];
        for name in [
            "JoinRequest",
            "ResumeRequest",
            "SessionResponse",
            "ErrorResponse",
        ] {
            assert!(schemas[name].is_object(), "{name} missing from components");
        }
    }
}
