use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{MatchedPath, Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{MethodFilter, post},
};
use hyper::header::{ALLOW, AUTHORIZATION, CONTENT_TYPE, LOCATION, WWW_AUTHENTICATE};
use pulsebeam_core::auth::{
    ProjectRegistry, TokenError, VerifiedAuthorization, verify_participant_token,
};
use pulsebeam_runtime::mailbox::TrySendError;
use serde::{Deserialize, Serialize};
use str0m::{change::SdpOffer, error::SdpError};
use tokio::time::Instant;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use crate::{
    control::controller::{
        self, ConnectionProfile, ControllerSender, CreateParticipantReply, ParticipantState,
    },
    entity::{ConnectionId, IdValidationError, ParticipantId, RoomId},
};

const MAX_SIGNALING_BODY_BYTES: usize = 1_048_576;
const ACCEPT_POST: hyper::header::HeaderName =
    hyper::header::HeaderName::from_static("accept-post");

#[derive(Clone)]
struct AppState {
    controller: ControllerSender,
    api_config: ApiConfig,
    project_registry: ProjectRegistry,
}

#[derive(Clone)]
pub struct ApiConfig {
    pub base_path: String,
    pub default_host: String,
}

#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("authorization bearer token required")]
    AuthorizationRequired,
    #[error(transparent)]
    Authorization(#[from] TokenError),
    #[error("invalid entity id format: {0}")]
    IdValidation(#[from] IdValidationError),
    #[error("sdp offer is invalid: {0}")]
    OfferInvalid(#[from] SdpError),
    #[error("join failed: {0}")]
    JoinError(#[from] controller::ControllerError),
    #[error("request body exceeds the 1 MiB limit")]
    PayloadTooLarge,
    #[error("too many requests, please try again later")]
    RateLimited,
    #[error("server is busy, please try again later")]
    ServiceUnavailable,
    #[error("failed to construct response URL")]
    BadUrl,
    #[error("resource not found")]
    NotFound,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("{0}")]
    Unknown(String),
}

#[derive(Serialize, ToSchema)]
struct Problem {
    r#type: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, title) = match &self {
            Self::AuthorizationRequired
            | Self::Authorization(_)
            | Self::JoinError(controller::ControllerError::AuthorizationExpired) => {
                (StatusCode::UNAUTHORIZED, "Unauthorized")
            }
            Self::IdValidation(_)
            | Self::OfferInvalid(_)
            | Self::JoinError(controller::ControllerError::OfferRejected(_))
            | Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "Bad Request"),
            Self::NotFound => (StatusCode::NOT_FOUND, "Not Found"),
            Self::JoinError(controller::ControllerError::Superseded) => {
                (StatusCode::CONFLICT, "Conflict")
            }
            Self::PayloadTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large"),
            Self::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "Too Many Requests"),
            Self::JoinError(controller::ControllerError::ServiceUnavailable)
            | Self::ServiceUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable"),
            Self::JoinError(controller::ControllerError::Unknown(_))
            | Self::JoinError(controller::ControllerError::IOError(_))
            | Self::BadUrl
            | Self::Unknown(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
        };
        let challenge = (status == StatusCode::UNAUTHORIZED).then(|| {
            if matches!(self, Self::AuthorizationRequired) {
                HeaderValue::from_static("Bearer")
            } else {
                HeaderValue::from_static("Bearer error=\"invalid_token\"")
            }
        });
        let detail = if status == StatusCode::UNAUTHORIZED {
            title.to_owned()
        } else {
            self.to_string()
        };
        let mut response = (
            status,
            [(CONTENT_TYPE, "application/problem+json")],
            Json(Problem {
                r#type: "about:blank",
                title,
                status: status.as_u16(),
                detail,
            }),
        )
            .into_response();
        if let Some(challenge) = challenge {
            response.headers_mut().insert(WWW_AUTHENTICATE, challenge);
        }
        response
    }
}

pub(crate) fn verify_bearer(
    headers: &HeaderMap,
    registry: &ProjectRegistry,
    now: u64,
) -> Result<VerifiedAuthorization, ApiError> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next().ok_or(ApiError::AuthorizationRequired)?;
    if values.next().is_some() {
        return Err(TokenError::Malformed.into());
    }
    let value = value.to_str().map_err(|_| TokenError::Malformed)?;
    let (scheme, token) = value.split_once(' ').ok_or(TokenError::Malformed)?;
    if !scheme.eq_ignore_ascii_case("Bearer")
        || token.is_empty()
        || token.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(TokenError::Malformed.into());
    }
    verify_participant_token(registry, token, now).map_err(Into::into)
}

fn build_location(headers: &HeaderMap, cfg: &ApiConfig, path: &str) -> Result<String, ApiError> {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or(&cfg.default_host);
    let url = format!("{scheme}://{host}{}{path}", cfg.base_path);
    url.parse::<Uri>().map_err(|_| ApiError::BadUrl)?;
    Ok(url)
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct NativeRequest {
    offer: String,
    #[serde(default)]
    manual: bool,
}

#[derive(Serialize, ToSchema)]
struct NativeResponse {
    room_external_id: String,
    #[schema(value_type = String)]
    room_id: RoomId,
    participant_external_id: String,
    #[schema(value_type = String)]
    participant_id: ParticipantId,
    #[schema(value_type = String)]
    connection_id: ConnectionId,
    answer: String,
}

fn ensure_json_content_type(headers: &HeaderMap) -> Result<(), ApiError> {
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    if is_json {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "Content-Type must be application/json".to_owned(),
        ))
    }
}

fn ensure_sdp_content_type(headers: &HeaderMap) -> Result<(), ApiError> {
    let is_sdp = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/sdp"));
    if is_sdp {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "Content-Type must be application/sdp".to_owned(),
        ))
    }
}

fn validate_directions(
    offer: &str,
    allowed: &[&str],
    required_description: &str,
    require_active_rtp: bool,
) -> Result<(), ApiError> {
    let mut active_rtp = false;
    let mut session_direction = None;
    let mut current_active_rtp = false;
    let mut current_direction = None;
    let mut in_media = false;
    let finish = |active: bool, direction: Option<&str>| -> Result<(), ApiError> {
        if active && !direction.is_some_and(|direction| allowed.contains(&direction)) {
            return Err(ApiError::BadRequest(format!(
                "active RTP media sections must be {required_description}"
            )));
        }
        Ok(())
    };
    for line in offer.lines().map(str::trim_end) {
        if let Some(media) = line.strip_prefix("m=") {
            finish(current_active_rtp, current_direction)?;
            let mut fields = media.split_ascii_whitespace();
            let _kind = fields.next();
            let port = fields.next();
            let protocol = fields.next();
            current_active_rtp = port.is_some_and(|value| value.split('/').next() != Some("0"))
                && protocol.is_some_and(|value| value.to_ascii_uppercase().contains("RTP"));
            active_rtp |= current_active_rtp;
            current_direction = session_direction;
            in_media = true;
        } else if let Some(direction) = line.strip_prefix("a=")
            && matches!(direction, "sendonly" | "recvonly" | "sendrecv" | "inactive")
        {
            current_direction = Some(direction);
            if !in_media {
                session_direction = Some(direction);
            }
        }
    }
    finish(current_active_rtp, current_direction)?;
    if require_active_rtp && !active_rtp {
        return Err(ApiError::BadRequest(
            "offer must contain an active RTP media section".to_owned(),
        ));
    }
    Ok(())
}

fn validate_native_directions(offer: &str) -> Result<(), ApiError> {
    validate_directions(
        offer,
        &["sendonly", "recvonly"],
        "sendonly or recvonly",
        false,
    )
}

fn validate_strict_directions(offer: &str, required: &str) -> Result<(), ApiError> {
    validate_directions(offer, &[required], required, true)
}

#[utoipa::path(
    post,
    path = "/native",
    request_body(content = NativeRequest, content_type = "application/json"),
    responses(
        (status = 201, description = "Native connection created", body = NativeResponse,
            headers(("Location" = String, description = "Absolute native resource URL"))),
        (status = 400, description = "Invalid request or SDP", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Invalid bearer authorization", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Candidate was superseded", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "Request body is too large", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Controller is busy", body = Problem, content_type = "application/problem+json"),
        (status = 500, description = "Internal server error", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Service unavailable", body = Problem, content_type = "application/problem+json")
    ),
    security(("bearer_auth" = [])),
    tag = "native"
)]
async fn create_native(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    let headers = request.headers().clone();
    let wall_now = SystemTime::now();
    let unix_now = wall_now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let authorization = verify_bearer(&headers, &state.project_registry, unix_now)?;
    ensure_json_content_type(&headers)?;
    let body = to_bytes(request.into_body(), MAX_SIGNALING_BODY_BYTES)
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?;
    let request: NativeRequest = serde_json::from_slice(&body)
        .map_err(|error| ApiError::BadRequest(format!("invalid JSON request: {error}")))?;
    if request.offer.len() > MAX_SIGNALING_BODY_BYTES {
        return Err(ApiError::PayloadTooLarge);
    }
    let offer = SdpOffer::from_sdp_string(&request.offer)?;
    validate_native_directions(&request.offer)?;
    let lease = controller::AuthorizationLease::from_expiry(
        authorization.expiry,
        wall_now,
        tokio::time::Instant::now(),
    )?;
    let connection_id = ConnectionId::new();
    let participant_state = ParticipantState {
        manual_sub: request.manual,
        room_id: authorization.room_id,
        participant_id: authorization.participant_id,
        connection_id,
        old_connection_id: None,
        authorization: Some(lease),
        profile: ConnectionProfile::Native,
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state
        .controller
        .try_send(
            (
                controller::CreateParticipant {
                    state: participant_state,
                    offer,
                },
                reply_tx,
            )
                .into(),
        )
        .map_err(|error| match error {
            TrySendError::Full(_) => ApiError::RateLimited,
            TrySendError::Closed(_) => ApiError::ServiceUnavailable,
        })?;
    let reply: CreateParticipantReply = reply_rx
        .await
        .map_err(|_| controller::ControllerError::ServiceUnavailable)??;
    let location = build_location(
        &headers,
        &state.api_config,
        &format!("/native/{connection_id}"),
    )?;
    let location = HeaderValue::from_str(&location).map_err(|_| ApiError::BadUrl)?;
    Ok((
        StatusCode::CREATED,
        [(LOCATION, location)],
        Json(NativeResponse {
            room_external_id: authorization.room_external_id.as_str().to_owned(),
            room_id: authorization.room_id,
            participant_external_id: authorization.participant_external_id.as_str().to_owned(),
            participant_id: authorization.participant_id,
            connection_id,
            answer: reply.answer.to_sdp_string(),
        }),
    )
        .into_response())
}

#[utoipa::path(
    delete,
    path = "/native/{connection_id}",
    params(("connection_id" = String, Path, description = "Canonical connection ID")),
    responses(
        (status = 204, description = "Native connection deleted or already absent"),
        (status = 401, description = "Invalid bearer authorization", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Malformed connection ID", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Controller is busy", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Service unavailable", body = Problem, content_type = "application/problem+json")
    ),
    security(("bearer_auth" = [])),
    tag = "native"
)]
async fn delete_native(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let authorization = verify_bearer(&headers, &state.project_registry, now)?;
    let connection_id = connection_id.parse().map_err(|_| ApiError::NotFound)?;
    state
        .controller
        .try_send(
            controller::DeleteParticipant {
                room_id: authorization.room_id,
                participant_id: authorization.participant_id,
                connection_id,
                profile: ConnectionProfile::Native,
            }
            .into(),
        )
        .map_err(|error| match error {
            TrySendError::Full(_) => ApiError::RateLimited,
            TrySendError::Closed(_) => ApiError::ServiceUnavailable,
        })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_sdp_profile(
    state: AppState,
    request: Request,
    profile: ConnectionProfile,
    required_direction: &'static str,
    resource: &'static str,
) -> Result<Response, ApiError> {
    let headers = request.headers().clone();
    let wall_now = SystemTime::now();
    let unix_now = wall_now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let authorization = verify_bearer(&headers, &state.project_registry, unix_now)?;
    ensure_sdp_content_type(&headers)?;
    let body = to_bytes(request.into_body(), MAX_SIGNALING_BODY_BYTES)
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?;
    let raw_offer = std::str::from_utf8(&body)
        .map_err(|_| ApiError::BadRequest("SDP body must be UTF-8".to_owned()))?;
    let offer = SdpOffer::from_sdp_string(raw_offer)?;
    validate_strict_directions(raw_offer, required_direction)?;
    let lease = controller::AuthorizationLease::from_expiry(
        authorization.expiry,
        wall_now,
        tokio::time::Instant::now(),
    )?;
    let connection_id = ConnectionId::new();
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state
        .controller
        .try_send(
            (
                controller::CreateParticipant {
                    state: ParticipantState {
                        manual_sub: false,
                        room_id: authorization.room_id,
                        participant_id: authorization.participant_id,
                        connection_id,
                        old_connection_id: None,
                        authorization: Some(lease),
                        profile,
                    },
                    offer,
                },
                reply_tx,
            )
                .into(),
        )
        .map_err(|error| match error {
            TrySendError::Full(_) => ApiError::RateLimited,
            TrySendError::Closed(_) => ApiError::ServiceUnavailable,
        })?;
    let reply: CreateParticipantReply = reply_rx
        .await
        .map_err(|_| controller::ControllerError::ServiceUnavailable)??;
    let location = build_location(
        &headers,
        &state.api_config,
        &format!("/{resource}/{connection_id}"),
    )?;
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        LOCATION,
        HeaderValue::from_str(&location).map_err(|_| ApiError::BadUrl)?,
    );
    response_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/sdp"));
    Ok((
        StatusCode::CREATED,
        response_headers,
        reply.answer.to_sdp_string(),
    )
        .into_response())
}

#[utoipa::path(
    post,
    path = "/whip",
    request_body(content = String, description = "Send-only WebRTC SDP offer", content_type = "application/sdp"),
    responses(
        (status = 201, description = "WHIP connection created", body = String, content_type = "application/sdp",
            headers(("Location" = String, description = "Absolute opaque WHIP resource URL"))),
        (status = 400, description = "Invalid request or SDP", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Invalid bearer authorization", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Candidate was superseded", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "Request body is too large", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Controller is busy", body = Problem, content_type = "application/problem+json"),
        (status = 500, description = "Internal server error", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Service unavailable", body = Problem, content_type = "application/problem+json")
    ),
    security(("bearer_auth" = [])),
    tag = "whip"
)]
async fn create_whip(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    create_sdp_profile(state, request, ConnectionProfile::Whip, "sendonly", "whip").await
}

#[utoipa::path(
    post,
    path = "/whep",
    request_body(content = String, description = "Receive-only WebRTC SDP offer", content_type = "application/sdp"),
    responses(
        (status = 201, description = "WHEP connection created", body = String, content_type = "application/sdp",
            headers(("Location" = String, description = "Absolute opaque WHEP resource URL"))),
        (status = 400, description = "Invalid request or SDP", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Invalid bearer authorization", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Candidate was superseded", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "Request body is too large", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Controller is busy", body = Problem, content_type = "application/problem+json"),
        (status = 500, description = "Internal server error", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Service unavailable", body = Problem, content_type = "application/problem+json")
    ),
    security(("bearer_auth" = [])),
    tag = "whep"
)]
async fn create_whep(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    create_sdp_profile(state, request, ConnectionProfile::Whep, "recvonly", "whep").await
}

fn verify_discovery(state: &AppState, headers: &HeaderMap) -> Result<StatusCode, ApiError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let _authorization = verify_bearer(headers, &state.project_registry, now)?;
    Ok(StatusCode::OK)
}

macro_rules! discovery_handler {
    ($name:ident, $method:ident, $path:literal, $tag:literal $(, $parameter:literal)?) => {
        #[utoipa::path(
            $method,
            path = $path,
            $(params(($parameter = String, Path, description = "Opaque connection resource identifier")),)?
            responses(
                (status = 200, description = "Authenticated discovery response"),
                (status = 401, description = "Invalid bearer authorization", body = Problem, content_type = "application/problem+json")
            ),
            security(("bearer_auth" = [])),
            tag = $tag
        )]
        async fn $name(
            State(state): State<AppState>,
            headers: HeaderMap,
        ) -> Result<StatusCode, ApiError> {
            verify_discovery(&state, &headers)
        }
    };
}

discovery_handler!(get_whip, get, "/whip", "whip");
discovery_handler!(
    get_whip_session,
    get,
    "/whip/{connection_id}",
    "whip",
    "connection_id"
);
discovery_handler!(get_whep, get, "/whep", "whep");
discovery_handler!(
    get_whep_session,
    get,
    "/whep/{connection_id}",
    "whep",
    "connection_id"
);
discovery_handler!(head_whep, head, "/whep", "whep");
discovery_handler!(
    head_whep_session,
    head,
    "/whep/{connection_id}",
    "whep",
    "connection_id"
);

fn discovery_options(allow: &'static str, accept_post: bool) -> (StatusCode, HeaderMap) {
    let mut headers = HeaderMap::new();
    headers.insert(ALLOW, HeaderValue::from_static(allow));
    if accept_post {
        headers.insert(ACCEPT_POST, HeaderValue::from_static("application/sdp"));
    }
    (StatusCode::OK, headers)
}

macro_rules! options_handler {
    ($name:ident, $path:literal, $tag:literal, $allow:literal, $accept_post:literal $(, $parameter:literal)?) => {
        #[utoipa::path(
                    options,
                    path = $path,
                    $(params(($parameter = String, Path, description = "Opaque connection resource identifier")),)?
                    responses(
                        (status = 200, description = "Protocol discovery",
                            headers(
                                ("Allow" = String, description = "Supported methods"),
                                ("Accept-Post" = String, description = "Supported POST media type")
                            ))
                    ),
                    tag = $tag
                )]
        async fn $name() -> (StatusCode, HeaderMap) {
            discovery_options($allow, $accept_post)
        }
    };
}

options_handler!(options_whip, "/whip", "whip", "POST, GET, OPTIONS", true);
options_handler!(
    options_whip_session,
    "/whip/{connection_id}",
    "whip",
    "GET, OPTIONS, DELETE",
    false,
    "connection_id"
);
options_handler!(
    options_whep,
    "/whep",
    "whep",
    "POST, GET, HEAD, OPTIONS",
    true
);
options_handler!(
    options_whep_session,
    "/whep/{connection_id}",
    "whep",
    "GET, HEAD, OPTIONS, DELETE",
    false,
    "connection_id"
);

async fn delete_sdp_profile(
    state: AppState,
    connection_id: String,
    headers: HeaderMap,
    profile: ConnectionProfile,
) -> Result<StatusCode, ApiError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let authorization = verify_bearer(&headers, &state.project_registry, now)?;
    let connection_id = connection_id.parse().map_err(|_| ApiError::NotFound)?;
    state
        .controller
        .try_send(
            controller::DeleteParticipant {
                room_id: authorization.room_id,
                participant_id: authorization.participant_id,
                connection_id,
                profile,
            }
            .into(),
        )
        .map_err(|error| match error {
            TrySendError::Full(_) => ApiError::RateLimited,
            TrySendError::Closed(_) => ApiError::ServiceUnavailable,
        })?;
    Ok(StatusCode::OK)
}

macro_rules! delete_handler {
    ($name:ident, $path:literal, $tag:literal, $profile:expr) => {
        #[utoipa::path(
            delete,
            path = $path,
            params(("connection_id" = String, Path, description = "Canonical connection ID")),
            responses(
                (status = 200, description = "Connection deleted or already absent"),
                (status = 401, description = "Invalid bearer authorization", body = Problem, content_type = "application/problem+json"),
                (status = 404, description = "Malformed connection ID", body = Problem, content_type = "application/problem+json"),
                (status = 429, description = "Controller is busy", body = Problem, content_type = "application/problem+json"),
                (status = 503, description = "Service unavailable", body = Problem, content_type = "application/problem+json")
            ),
            security(("bearer_auth" = [])),
            tag = $tag
        )]
        async fn $name(
            State(state): State<AppState>,
            Path(connection_id): Path<String>,
            headers: HeaderMap,
        ) -> Result<StatusCode, ApiError> {
            delete_sdp_profile(state, connection_id, headers, $profile).await
        }
    };
}

delete_handler!(
    delete_whip,
    "/whip/{connection_id}",
    "whip",
    ConnectionProfile::Whip
);
delete_handler!(
    delete_whep,
    "/whep/{connection_id}",
    "whep",
    ConnectionProfile::Whep
);

async fn method_not_allowed() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

fn build_openapi(base_path: &str) -> utoipa::openapi::OpenApi {
    use utoipa::openapi::{
        ContactBuilder, InfoBuilder, OpenApi as OpenApiSpec, ServerBuilder,
        security::{HttpAuthScheme, SecurityScheme},
    };
    let info = InfoBuilder::new()
        .title("PulseBeam API")
        .version("1.0.0")
        .description(Some("PulseBeam signaling resources"))
        .contact(Some(
            ContactBuilder::new()
                .name(Some("API Support"))
                .email(Some("lukas@pulsebeam.dev"))
                .build(),
        ))
        .build();
    let mut openapi = OpenApiSpec::new(info, utoipa::openapi::path::Paths::new());
    openapi.servers = Some(vec![
        ServerBuilder::new()
            .url(base_path)
            .description(Some("API Server"))
            .build(),
    ]);
    let generated = ApiDoc::openapi();
    openapi.paths = generated.paths;
    openapi.components = generated.components;
    openapi.tags = generated.tags;
    if let Some(components) = openapi.components.as_mut() {
        components.add_security_scheme(
            "bearer_auth",
            SecurityScheme::Http(utoipa::openapi::security::Http::new(HttpAuthScheme::Bearer)),
        );
    }
    openapi
}

#[derive(OpenApi)]
#[openapi(
    paths(
        create_native,
        delete_native,
        create_whip,
        get_whip,
        options_whip,
        get_whip_session,
        options_whip_session,
        delete_whip,
        create_whep,
        get_whep,
        head_whep,
        options_whep,
        get_whep_session,
        head_whep_session,
        options_whep_session,
        delete_whep
    ),
    components(schemas(NativeRequest, NativeResponse, Problem)),
    tags(
        (name = "native", description = "Native signaling resource"),
        (name = "whip", description = "Strict WHIP signaling resource"),
        (name = "whep", description = "Strict WHEP signaling resource")
    )
)]
struct ApiDoc;

pub fn router(
    controller: ControllerSender,
    cfg: ApiConfig,
    project_registry: ProjectRegistry,
) -> Router {
    let openapi = build_openapi(&cfg.base_path);
    let api = Router::new()
        .route("/native", post(create_native))
        .route(
            "/native/{connection_id}",
            axum::routing::delete(delete_native),
        )
        .route(
            "/whip",
            post(create_whip)
                .on(MethodFilter::GET, get_whip)
                .head(method_not_allowed)
                .options(options_whip),
        )
        .route(
            "/whip/{connection_id}",
            axum::routing::on(MethodFilter::GET, get_whip_session)
                .head(method_not_allowed)
                .options(options_whip_session)
                .delete(delete_whip),
        )
        .route(
            "/whep",
            post(create_whep)
                .on(MethodFilter::GET, get_whep)
                .head(head_whep)
                .options(options_whep),
        )
        .route(
            "/whep/{connection_id}",
            axum::routing::on(MethodFilter::GET, get_whep_session)
                .head(head_whep_session)
                .options(options_whep_session)
                .delete(delete_whep),
        )
        .layer(middleware::from_fn(track_route_duration));
    Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", openapi))
        .nest(&cfg.base_path, api)
        .with_state(AppState {
            controller,
            api_config: cfg,
            project_registry,
        })
}

async fn track_route_duration(req: Request<Body>, next: Next) -> Response {
    let start = Instant::now();
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_else(|| "unknown_route".to_owned());
    let method = req.method().to_string();
    let response = next.run(req).await;
    metrics::histogram!("http_request_duration_seconds", "path" => path, "method" => method)
        .record(start.elapsed().as_secs_f64());
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_core::{
        auth::{
            DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_VERIFYING_KEY, DEVELOPMENT_PROJECT_ID,
            ProjectKey, ProjectKeys, mint_development_token,
        },
        identity::{ParticipantExternalId, RoomExternalId},
    };
    use str0m::{
        RtcConfig,
        change::SdpAnswer,
        media::{Direction, MediaKind},
    };
    use tower::ServiceExt;
    use tower_http::cors::{Any, CorsLayer};

    fn auth_registry() -> ProjectRegistry {
        ProjectRegistry::new(vec![ProjectKeys {
            project_id: DEVELOPMENT_PROJECT_ID,
            keys: vec![ProjectKey {
                key_id: DEVELOPMENT_API_KEY_ID,
                verifying_key: DEVELOPMENT_API_VERIFYING_KEY,
            }],
        }])
        .unwrap()
    }

    fn authorization_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    fn cfg() -> ApiConfig {
        ApiConfig {
            base_path: "/api/v1".to_owned(),
            default_host: "localhost:7070".to_owned(),
        }
    }

    fn valid_offer() -> String {
        let mut rtc = RtcConfig::new().build(std::time::Instant::now());
        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        change.apply().unwrap().0.to_sdp_string()
    }

    fn directional_offer(direction: Direction) -> String {
        let mut rtc = RtcConfig::new().build(std::time::Instant::now());
        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Audio, direction, None, None, None);
        change.apply().unwrap().0.to_sdp_string()
    }

    fn live_token() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        mint_development_token(
            &RoomExternalId::new("general").unwrap(),
            &ParticipantExternalId::new("alice").unwrap(),
            now + 60,
        )
        .unwrap()
    }

    fn native_post(token: &str, body: Vec<u8>) -> Request {
        Request::builder()
            .method("POST")
            .uri("/api/v1/native")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(CONTENT_TYPE, "application/json")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "media.example")
            .body(Body::from(body))
            .unwrap()
    }

    fn sdp_post(token: &str, profile: &str, body: Vec<u8>) -> Request {
        Request::builder()
            .method("POST")
            .uri(format!("/api/v1/{profile}"))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(CONTENT_TYPE, "application/sdp")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "media.example")
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn native_post_returns_claim_identity_answer_and_absolute_location() {
        let (controller, mut commands) = pulsebeam_runtime::mailbox::new(4);
        let app = router(controller, cfg(), auth_registry());
        let token = live_token();
        let body = serde_json::to_vec(&serde_json::json!({
            "offer": valid_offer(),
            "manual": true
        }))
        .unwrap();
        let responder = tokio::spawn(async move {
            let controller::ControllerCommand::CreateParticipant(message, reply) =
                commands.recv().await.unwrap()
            else {
                panic!("expected create command")
            };
            assert!(message.state.manual_sub);
            assert_eq!(message.state.profile, ConnectionProfile::Native);
            assert!(message.state.authorization.is_some());
            let answer = SdpAnswer::from_sdp_string(&message.offer.to_sdp_string()).unwrap();
            reply.send(Ok(CreateParticipantReply { answer })).unwrap();
        });
        let response = app.oneshot(native_post(&token, body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let location = response.headers()[LOCATION].to_str().unwrap().to_owned();
        assert!(location.starts_with("https://media.example/api/v1/native/c_0"));
        assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
        let body = to_bytes(response.into_body(), MAX_SIGNALING_BODY_BYTES)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["room_external_id"], "general");
        assert_eq!(body["participant_external_id"], "alice");
        let room_id = RoomId::derive(
            &DEVELOPMENT_PROJECT_ID,
            &RoomExternalId::new("general").unwrap(),
        );
        let participant_id =
            ParticipantId::derive(&room_id, &ParticipantExternalId::new("alice").unwrap());
        assert_eq!(body["room_id"], room_id.as_str());
        assert_eq!(body["participant_id"], participant_id.as_str());
        assert!(body["connection_id"].as_str().unwrap().starts_with("c_0"));
        assert!(body["answer"].as_str().unwrap().starts_with("v=0"));
        assert!(location.ends_with(body["connection_id"].as_str().unwrap()));
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn native_post_rejects_malformed_contract_before_controller_allocation() {
        let token = live_token();
        let cases = [
            serde_json::to_vec(&serde_json::json!({"offer": valid_offer(), "unknown": true}))
                .unwrap(),
            b"{".to_vec(),
            vec![b' '; MAX_SIGNALING_BODY_BYTES + 1],
        ];
        for body in cases {
            let (controller, mut commands) = pulsebeam_runtime::mailbox::new(1);
            let response = router(controller, cfg(), auth_registry())
                .oneshot(native_post(&token, body))
                .await
                .unwrap();
            assert!(
                matches!(
                    response.status(),
                    StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
                ),
                "unexpected status {}",
                response.status()
            );
            assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
            assert!(commands.try_recv().is_err());
        }

        let (controller, _) = pulsebeam_runtime::mailbox::new(1);
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/native")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from("{}"))
            .unwrap();
        let response = router(controller, cfg(), auth_registry())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn native_delete_uses_claim_identity_profile_and_path_connection() {
        let (controller, mut commands) = pulsebeam_runtime::mailbox::new(1);
        let token = live_token();
        let connection_id = ConnectionId::new();
        let request = Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/native/{connection_id}"))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router(controller, cfg(), auth_registry())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let controller::ControllerCommand::DeleteParticipant(message) =
            commands.recv().await.unwrap()
        else {
            panic!("expected delete command")
        };
        assert_eq!(message.connection_id, connection_id);
        assert_eq!(message.profile, ConnectionProfile::Native);

        let (controller, _) = pulsebeam_runtime::mailbox::new(1);
        let malformed = Request::builder()
            .method("DELETE")
            .uri("/api/v1/native/not-a-connection")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router(controller, cfg(), auth_registry())
            .oneshot(malformed)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
    }

    #[tokio::test]
    async fn strict_sdp_posts_return_raw_answers_and_opaque_locations() {
        for (resource, direction, profile) in [
            ("whip", Direction::SendOnly, ConnectionProfile::Whip),
            ("whep", Direction::RecvOnly, ConnectionProfile::Whep),
        ] {
            let (controller, mut commands) = pulsebeam_runtime::mailbox::new(1);
            let app = router(controller, cfg(), auth_registry());
            let token = live_token();
            let offer = directional_offer(direction);
            let answer = offer.clone();
            let responder = tokio::spawn(async move {
                let controller::ControllerCommand::CreateParticipant(message, reply) =
                    commands.recv().await.unwrap()
                else {
                    panic!("expected create command")
                };
                assert_eq!(message.state.profile, profile);
                assert!(!message.state.manual_sub);
                assert!(message.state.authorization.is_some());
                reply
                    .send(Ok(CreateParticipantReply {
                        answer: SdpAnswer::from_sdp_string(&answer).unwrap(),
                    }))
                    .unwrap();
            });
            let response = app
                .oneshot(sdp_post(&token, resource, offer.into_bytes()))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(response.headers()[CONTENT_TYPE], "application/sdp");
            assert!(!response.headers().contains_key("etag"));
            assert!(!response.headers().contains_key("pb-participant-id"));
            let location = response.headers()[LOCATION].to_str().unwrap();
            assert!(location.starts_with(&format!("https://media.example/api/v1/{resource}/c_0")));
            let body = to_bytes(response.into_body(), MAX_SIGNALING_BODY_BYTES)
                .await
                .unwrap();
            assert!(body.starts_with(b"v=0"));
            responder.await.unwrap();
        }
    }

    #[tokio::test]
    async fn strict_sdp_posts_enforce_content_type_and_body_limit() {
        let token = live_token();
        let cases = [
            Request::builder()
                .method("POST")
                .uri("/api/v1/whip")
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(directional_offer(Direction::SendOnly)))
                .unwrap(),
            sdp_post(&token, "whep", vec![b' '; MAX_SIGNALING_BODY_BYTES + 1]),
        ];
        for request in cases {
            let (controller, mut commands) = pulsebeam_runtime::mailbox::new(1);
            let response = router(controller, cfg(), auth_registry())
                .oneshot(request)
                .await
                .unwrap();
            assert!(matches!(
                response.status(),
                StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
            ));
            assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
            assert!(commands.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn whip_and_whep_delete_use_claim_identity_path_and_profile() {
        for (resource, profile) in [
            ("whip", ConnectionProfile::Whip),
            ("whep", ConnectionProfile::Whep),
        ] {
            let (controller, mut commands) = pulsebeam_runtime::mailbox::new(1);
            let token = live_token();
            let connection_id = ConnectionId::new();
            let request = Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/{resource}/{connection_id}"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let response = router(controller, cfg(), auth_registry())
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let controller::ControllerCommand::DeleteParticipant(message) =
                commands.recv().await.unwrap()
            else {
                panic!("expected delete command")
            };
            assert_eq!(message.connection_id, connection_id);
            assert_eq!(message.profile, profile);
        }

        let token = live_token();
        for resource in ["whip", "whep"] {
            let (controller, _) = pulsebeam_runtime::mailbox::new(1);
            let malformed = Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/{resource}/not-a-connection"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                router(controller, cfg(), auth_registry())
                    .oneshot(malformed)
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
    }

    #[test]
    fn strict_request_defaults_manual_and_rejects_unknown_fields() {
        let automatic: NativeRequest = serde_json::from_str(r#"{"offer":"v=0"}"#).unwrap();
        assert!(!automatic.manual);
        let manual: NativeRequest =
            serde_json::from_str(r#"{"offer":"v=0","manual":true}"#).unwrap();
        assert!(manual.manual);
        assert!(
            serde_json::from_str::<NativeRequest>(r#"{"offer":"v=0","room":"other"}"#).is_err()
        );
    }

    #[test]
    fn native_direction_profile_accepts_mixed_unidirectional_media() {
        let valid = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n\
                     m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=recvonly\r\n\
                     m=audio 0 UDP/TLS/RTP/SAVPF 111\r\na=inactive\r\n";
        assert!(validate_native_directions(valid).is_ok());
        assert!(
            validate_native_directions("a=sendonly\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n").is_ok()
        );
        for direction in ["sendrecv", "inactive"] {
            let invalid = format!("m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na={direction}\r\n");
            assert!(validate_native_directions(&invalid).is_err());
        }
        assert!(
            validate_native_directions(
                "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=sendrecv\r\n"
            )
            .is_ok()
        );
    }

    #[test]
    fn strict_direction_profiles_cover_active_and_rejected_sections() {
        for (required, opposite) in [("sendonly", "recvonly"), ("recvonly", "sendonly")] {
            let valid = format!(
                "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na={required}\r\n\
                 m=video 0 UDP/TLS/RTP/SAVPF 96\r\na={opposite}\r\n"
            );
            assert!(validate_strict_directions(&valid, required).is_ok());
            let inherited = format!("a={required}\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n");
            assert!(validate_strict_directions(&inherited, required).is_ok());
            for rejected in [opposite, "sendrecv", "inactive"] {
                let invalid = format!("m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na={rejected}\r\n");
                assert!(validate_strict_directions(&invalid, required).is_err());
            }
            for port in ["0", "0/2"] {
                let only_port_zero =
                    format!("m=audio {port} UDP/TLS/RTP/SAVPF 111\r\na={required}\r\n");
                assert!(validate_strict_directions(&only_port_zero, required).is_err());
            }
        }
    }

    #[test]
    fn location_uses_proxy_host_host_and_fallback() {
        let mut proxy = HeaderMap::new();
        proxy.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        proxy.insert("x-forwarded-host", HeaderValue::from_static("sfu.example"));
        assert_eq!(
            build_location(&proxy, &cfg(), "/native/c").unwrap(),
            "https://sfu.example/api/v1/native/c"
        );
        let mut direct = HeaderMap::new();
        direct.insert("host", HeaderValue::from_static("media.example"));
        assert_eq!(
            build_location(&direct, &cfg(), "/native/c").unwrap(),
            "http://media.example/api/v1/native/c"
        );
        assert_eq!(
            build_location(&HeaderMap::new(), &cfg(), "/native/c").unwrap(),
            "http://localhost:7070/api/v1/native/c"
        );
    }

    #[tokio::test]
    async fn authorization_failures_are_secret_safe_rfc_9457_responses() {
        let token = mint_development_token(
            &RoomExternalId::new("general").unwrap(),
            &ParticipantExternalId::new("alice").unwrap(),
            2_000,
        )
        .unwrap();
        let cases = [
            (
                verify_bearer(&HeaderMap::new(), &auth_registry(), 1_000).unwrap_err(),
                "Bearer",
            ),
            (
                verify_bearer(
                    &authorization_headers("not-a-token"),
                    &auth_registry(),
                    1_000,
                )
                .unwrap_err(),
                "Bearer error=\"invalid_token\"",
            ),
            (
                verify_bearer(&authorization_headers(&token), &auth_registry(), 2_000).unwrap_err(),
                "Bearer error=\"invalid_token\"",
            ),
        ];
        for (error, challenge) in cases {
            assert!(!format!("{error:?}").contains(&token));
            let response = error.into_response();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(response.headers()[WWW_AUTHENTICATE], challenge);
            assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
            let body = to_bytes(response.into_body(), 1024).await.unwrap();
            assert!(!String::from_utf8(body.to_vec()).unwrap().contains(&token));
        }
    }

    #[tokio::test]
    async fn strict_profile_discovery_has_exact_methods_and_authentication() {
        let (controller, _) = pulsebeam_runtime::mailbox::new(1);
        let app = router(controller, cfg(), auth_registry());
        let token = live_token();
        for uri in [
            "/api/v1/whip",
            "/api/v1/whip/opaque",
            "/api/v1/whep",
            "/api/v1/whep/opaque",
        ] {
            let authenticated = Request::builder()
                .method("GET")
                .uri(uri)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(authenticated).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(to_bytes(response.into_body(), 1).await.unwrap().len(), 0);

            let unauthorized = Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                app.clone().oneshot(unauthorized).await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }

        for (uri, allow, accept_post) in [
            ("/api/v1/whip", "POST, GET, OPTIONS", true),
            ("/api/v1/whip/opaque", "GET, OPTIONS, DELETE", false),
            ("/api/v1/whep", "POST, GET, HEAD, OPTIONS", true),
            ("/api/v1/whep/opaque", "GET, HEAD, OPTIONS, DELETE", false),
        ] {
            let request = Request::builder()
                .method("OPTIONS")
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[ALLOW], allow);
            assert_eq!(response.headers().contains_key(&ACCEPT_POST), accept_post);
            if accept_post {
                assert_eq!(response.headers()[&ACCEPT_POST], "application/sdp");
            }
        }

        for (method, uri, expected) in [
            ("HEAD", "/api/v1/whip", StatusCode::METHOD_NOT_ALLOWED),
            ("HEAD", "/api/v1/whep", StatusCode::OK),
            (
                "PATCH",
                "/api/v1/whip/opaque",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                "PATCH",
                "/api/v1/whep/opaque",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected, "{method} {uri}");
            assert!(!response.headers().contains_key("etag"));
        }
    }

    #[test]
    fn openapi_contains_only_native_whip_and_whep_signaling_routes() {
        let openapi = build_openapi("/api/v1");
        let paths: Vec<_> = openapi.paths.paths.keys().cloned().collect();
        assert_eq!(
            paths,
            [
                "/native",
                "/native/{connection_id}",
                "/whep",
                "/whep/{connection_id}",
                "/whip",
                "/whip/{connection_id}"
            ]
        );
        let document = serde_json::to_value(&openapi).unwrap();
        for (path, methods) in [
            ("/whip", &["get", "options", "post"][..]),
            ("/whip/{connection_id}", &["delete", "get", "options"][..]),
            ("/whep", &["get", "head", "options", "post"][..]),
            (
                "/whep/{connection_id}",
                &["delete", "get", "head", "options"][..],
            ),
        ] {
            let operations = document["paths"][path].as_object().unwrap();
            let mut actual_methods = operations.keys().map(String::as_str).collect::<Vec<_>>();
            actual_methods.sort_unstable();
            assert_eq!(actual_methods, methods);
        }
        for profile in ["whip", "whep"] {
            let post = &document["paths"][format!("/{profile}")]["post"];
            assert!(post["requestBody"]["content"]["application/sdp"].is_object());
            assert!(post["responses"]["201"]["content"]["application/sdp"].is_object());
            assert!(post["responses"]["201"]["headers"]["Location"].is_object());
            assert!(post["responses"]["401"]["content"]["application/problem+json"].is_object());
            assert_eq!(post["security"][0]["bearer_auth"], serde_json::json!([]));
        }
        let json = serde_json::to_string(&document).unwrap();
        for old in [
            "/rooms",
            "pb-participant-id",
            "If-Match",
            "ETag",
            "\"patch\"",
        ] {
            assert!(
                !json.contains(old),
                "old contract remained in OpenAPI: {old}"
            );
        }
    }

    #[tokio::test]
    async fn router_exposes_native_without_legacy_signaling_routes() {
        let (controller, _) = pulsebeam_runtime::mailbox::new(1);
        let app = router(controller, cfg(), auth_registry());
        let native = Request::builder()
            .method("POST")
            .uri("/api/v1/native")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(native).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        for uri in [
            "/api/v1/rooms/general/participants",
            "/api/v1/rooms/general/participants/participant",
        ] {
            let request = Request::builder()
                .method("POST")
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::NOT_FOUND
            );
        }
    }

    #[tokio::test]
    async fn native_options_remains_available_through_cors() {
        let (controller, _) = pulsebeam_runtime::mailbox::new(1);
        let app = router(controller, cfg(), auth_registry()).layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([hyper::Method::POST, hyper::Method::DELETE])
                .allow_headers([AUTHORIZATION, CONTENT_TYPE]),
        );
        let request = Request::builder()
            .method("OPTIONS")
            .uri("/api/v1/native")
            .header("origin", "https://app.example")
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .contains_key("access-control-allow-methods")
        );
    }

    #[test]
    fn superseded_candidates_are_conflicts() {
        assert_eq!(
            ApiError::JoinError(controller::ControllerError::Superseded)
                .into_response()
                .status(),
            StatusCode::CONFLICT
        );
    }
}
