mod admission;
pub(crate) mod creation;

use std::time::{SystemTime, UNIX_EPOCH};

use super::MAX_NATIVE_SDP_BYTES;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{MatchedPath, Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{MethodFilter, post},
};
use hyper::header::{
    ALLOW, AUTHORIZATION, CONTENT_TYPE, ETAG, IF_MATCH, LOCATION, RETRY_AFTER, WWW_AUTHENTICATE,
};
use pulsebeam_core::auth::{
    KeyRegistry, TokenError, VerifiedAuthorization, VerifiedPrincipal,
    authenticate_participant_token,
};
use pulsebeam_queue::TrySendError;
use pulsebeam_runtime::time::Instant;
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use crate::{
    control::controller::{
        self, ControllerSender, CreateParticipantReply, ParticipantState, SignalingProtocol,
    },
    entity::{ConnectionId, IdValidationError, ParticipantId},
};

const MAX_SIGNALING_BODY_BYTES: usize = 1_048_576;
const MAX_NATIVE_BODY_BYTES: usize = 262_144;
const ACCEPT_POST: hyper::header::HeaderName =
    hyper::header::HeaderName::from_static("accept-post");

#[derive(Clone)]
struct AppState {
    controller: ControllerSender,
    api_config: ApiConfig,
    key_registry: KeyRegistry,
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
    #[error("join failed: {0}")]
    JoinError(#[from] controller::ControllerError),
    #[error("request body exceeds signaling limits")]
    PayloadTooLarge,
    #[error("too many requests, please try again later")]
    RateLimited,
    #[error("server is busy, please try again later")]
    ServiceUnavailable,
    #[error("failed to construct response URL")]
    BadUrl,
    #[error("resource not found")]
    NotFound,
    #[error("identity response encoding is required")]
    NotAcceptable,
    #[error("ownership recovery is unavailable")]
    RecoveryUnavailable,
    #[error("ownership attempt was superseded")]
    OwnershipSuperseded,
    #[error("authorization precondition required")]
    PreconditionRequired,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("{0}")]
    Unknown(String),
}

#[derive(Clone, Debug, Serialize, ToSchema)]
struct Problem {
    r#type: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ProblemResponse {
    status: StatusCode,
    headers: HeaderMap,
    problem: Option<Problem>,
}

impl ProblemResponse {
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }
}

impl IntoResponse for ProblemResponse {
    fn into_response(self) -> Response {
        match self.problem {
            Some(problem) => (self.status, self.headers, Json(problem)).into_response(),
            None => self.status.into_response(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.into_problem().into_response()
    }
}

impl ApiError {
    pub(crate) fn into_problem(self) -> ProblemResponse {
        let (status, title) = match &self {
            Self::AuthorizationRequired
            | Self::Authorization(_)
            | Self::JoinError(controller::ControllerError::AuthorizationExpired) => {
                (StatusCode::UNAUTHORIZED, "Unauthorized")
            }
            Self::JoinError(controller::ControllerError::OfferRejected(
                crate::control::negotiator::NegotiatorError::Initialization(_),
            ))
            | Self::JoinError(controller::ControllerError::Unknown(_))
            | Self::JoinError(controller::ControllerError::IOError(_))
            | Self::BadUrl
            | Self::Unknown(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
            Self::IdValidation(_)
            | Self::JoinError(controller::ControllerError::OfferRejected(_))
            | Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "Bad Request"),
            Self::NotFound => (StatusCode::NOT_FOUND, "Not Found"),
            Self::NotAcceptable => (StatusCode::NOT_ACCEPTABLE, "Not Acceptable"),
            Self::PreconditionRequired => {
                (StatusCode::PRECONDITION_REQUIRED, "Precondition Required")
            }
            Self::JoinError(controller::ControllerError::GrantGone) | Self::RecoveryUnavailable => {
                (StatusCode::GONE, "Gone")
            }
            Self::JoinError(controller::ControllerError::AuthorizationRejected) => {
                (StatusCode::FORBIDDEN, "Forbidden")
            }
            Self::JoinError(controller::ControllerError::AuthorizationPrecondition { .. }) => {
                (StatusCode::PRECONDITION_FAILED, "Precondition Failed")
            }
            Self::JoinError(controller::ControllerError::Superseded)
            | Self::OwnershipSuperseded => (StatusCode::CONFLICT, "Conflict"),
            Self::PayloadTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large"),
            Self::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "Too Many Requests"),
            Self::JoinError(controller::ControllerError::ServiceUnavailable)
            | Self::ServiceUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable"),
        };
        let etag = match &self {
            Self::JoinError(controller::ControllerError::AuthorizationPrecondition {
                revision,
            }) => match grant_etag(*revision) {
                Ok(etag) => Some(etag),
                Err(_) => {
                    return ProblemResponse {
                        status: StatusCode::INTERNAL_SERVER_ERROR,
                        headers: HeaderMap::new(),
                        problem: None,
                    };
                }
            },
            _ => None,
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
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        if let Some(challenge) = challenge {
            headers.insert(WWW_AUTHENTICATE, challenge);
        }
        if let Some(etag) = etag {
            headers.insert(ETAG, etag);
        }
        if matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
        ) {
            headers.insert(RETRY_AFTER, HeaderValue::from_static("1"));
        }
        ProblemResponse {
            status,
            headers,
            problem: Some(Problem {
                r#type: "about:blank",
                title,
                status: status.as_u16(),
                detail,
            }),
        }
    }
}

impl From<controller::OwnedAdmissionError> for ProblemResponse {
    fn from(error: controller::OwnedAdmissionError) -> Self {
        use controller::{OwnedAdmissionError, ownership::Error};
        match error {
            OwnedAdmissionError::Request(error) => ApiError::JoinError(error),
            OwnedAdmissionError::Authority(Error::UnknownProof | Error::ReplayUnavailable) => {
                ApiError::RecoveryUnavailable
            }
            OwnedAdmissionError::Authority(Error::Replaced | Error::StaleAttempt) => {
                ApiError::OwnershipSuperseded
            }
            OwnedAdmissionError::Authority(Error::InvalidProof | Error::ChangedAttempt) => {
                ApiError::BadRequest("invalid ownership attempt".to_owned())
            }
            OwnedAdmissionError::Authority(Error::Capacity | Error::QuiescencePending) => {
                ApiError::ServiceUnavailable
            }
            OwnedAdmissionError::Authority(Error::ResultTooLarge)
            | OwnedAdmissionError::Image(creation::Error::TooLarge) => ApiError::PayloadTooLarge,
            OwnedAdmissionError::Authority(Error::InvalidTransition)
            | OwnedAdmissionError::Image(
                creation::Error::Serialization | creation::Error::InvalidImage,
            ) => ApiError::Unknown("owned admission failed".to_owned()),
        }
        .into_problem()
    }
}

impl From<creation::PreflightError> for ApiError {
    fn from(error: creation::PreflightError) -> Self {
        match error {
            creation::PreflightError::IdentityRequired => Self::NotAcceptable,
            error => Self::BadRequest(error.to_string()),
        }
    }
}

#[cfg(test)]
fn verify_bearer(
    headers: &HeaderMap,
    registry: &KeyRegistry,
    now: u64,
) -> Result<VerifiedAuthorization, ApiError> {
    authenticate_bearer(headers, registry, now)?
        .authorize_room(pulsebeam_core::identity::RoomId::new("general")?)
        .map_err(|_| controller::ControllerError::AuthorizationRejected.into())
}

fn authenticate_bearer(
    headers: &HeaderMap,
    registry: &KeyRegistry,
    now: u64,
) -> Result<VerifiedPrincipal, ApiError> {
    authenticate_participant_token(registry, bearer_token(headers)?, now).map_err(Into::into)
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, ApiError> {
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
    Ok(token)
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
    #[schema(example = "pulsebeam.media.v1")]
    profile: String,
    offer: String,
    room: String,
}

fn parse_native_request(body: &[u8]) -> Result<NativeRequest, ApiError> {
    let invalid = || ApiError::BadRequest("invalid native JSON request".to_owned());
    if body.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(invalid());
    }
    let request: NativeRequest = serde_json::from_slice(body).map_err(|_| invalid())?;
    pulsebeam_core::identity::RoomId::new(&request.room)
        .map_err(|_| ApiError::BadRequest("invalid room external id".to_owned()))?;
    if request.profile != pulsebeam_proto::state::PROFILE {
        return Err(ApiError::BadRequest(
            "unsupported native profile".to_owned(),
        ));
    }
    Ok(request)
}

#[derive(Serialize, ToSchema)]
struct NativeResponse<'a> {
    profile: &'static str,
    #[schema(value_type = Object)]
    rtp_capabilities: &'a pulsebeam_proto::state::RtpCapabilities,
    #[schema(value_type = Vec<String>)]
    data_delivery: [pulsebeam_proto::state::DataDelivery; 3],
    room_id: &'a str,
    #[schema(value_type = String)]
    participant_id: ParticipantId,
    #[schema(value_type = String)]
    connection_id: ConnectionId,
    answer: &'a str,
}

impl<'a> NativeResponse<'a> {
    fn new(
        authorization: &'a VerifiedAuthorization,
        connection_id: ConnectionId,
        rtp_capabilities: &'a pulsebeam_proto::state::RtpCapabilities,
        answer: &'a str,
    ) -> Self {
        Self {
            profile: pulsebeam_proto::state::PROFILE,
            rtp_capabilities,
            data_delivery: [
                pulsebeam_proto::state::DataDelivery::Latest,
                pulsebeam_proto::state::DataDelivery::Lossy,
                pulsebeam_proto::state::DataDelivery::Ordered,
            ],
            room_id: authorization.room_id.as_str(),
            participant_id: authorization.participant_id,
            connection_id,
            answer,
        }
    }
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

fn validate_single_media_per_kind(offer: &str) -> Result<(), ApiError> {
    let (mut audio, mut video) = (0_u8, 0_u8);
    for line in offer.lines().map(str::trim_end) {
        let Some(media) = line.strip_prefix("m=") else {
            continue;
        };
        let mut fields = media.split_ascii_whitespace();
        let kind = fields.next();
        let port = fields.next();
        let protocol = fields.next();
        if port.is_none_or(|value| value.split('/').next() == Some("0"))
            || !protocol.is_some_and(|value| value.to_ascii_uppercase().contains("RTP"))
        {
            continue;
        }
        match kind {
            Some("audio") => audio = audio.saturating_add(1),
            Some("video") => video = video.saturating_add(1),
            _ => {}
        }
        if audio > 1 || video > 1 {
            return Err(ApiError::BadRequest(
                "WHIP/WHEP support at most one active track per media kind".to_owned(),
            ));
        }
    }
    Ok(())
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
async fn create_native(state: State<AppState>, request: Request) -> Result<Response, ApiError> {
    admission::create(state, request).await
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct NativeAdmissionRequest {}

#[derive(Serialize, ToSchema)]
struct NativeAdmissionResponse {
    owner: String,
}

#[utoipa::path(
    post,
    path = "/native/admissions",
    request_body(content = NativeAdmissionRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Fresh principal ownership capability", body = NativeAdmissionResponse),
        (status = 400, description = "Invalid request", body = Problem),
        (status = 401, description = "Invalid bearer authorization", body = Problem),
        (status = 503, description = "Ownership authority unavailable", body = Problem)
    ),
    security(("bearer_auth" = [])),
    tag = "native"
)]
async fn issue_native_admission(
    state: State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    admission::issue(state, request).await
}

fn native_message(
    authorization: &VerifiedAuthorization,
    offer: String,
    wall_now: SystemTime,
) -> Result<
    (
        controller::CreateParticipant,
        pulsebeam_proto::state::RtpCapabilities,
    ),
    ApiError,
> {
    if offer.len() > MAX_NATIVE_SDP_BYTES {
        return Err(ApiError::PayloadTooLarge);
    }
    validate_native_directions(&offer)?;
    let lease = controller::AuthorizationLease::from_verified(
        authorization.clone(),
        wall_now,
        Instant::now(),
    )?;
    lease.native_expiry()?;
    let rtp_capabilities = crate::control::rtp::capabilities();
    let message = controller::CreateParticipant {
        state: ParticipantState {
            manual_sub: true,
            room_id: authorization.room_id,
            participant_id: authorization.participant_id,
            connection_id: ConnectionId::new(),
            old_connection_id: None,
            authorization: Some(lease),
            profile: SignalingProtocol::State,
            rtp_capabilities: Some(rtp_capabilities.clone()),
        },
        offer,
    };
    Ok((message, rtp_capabilities))
}

#[cfg(test)]
pub(super) fn owned_native_router(
    controller: ControllerSender,
    cfg: ApiConfig,
    registry: KeyRegistry,
) -> Router {
    router(controller, cfg, registry)
}

fn grant_etag(revision: u64) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(&format!("\"auth-{revision}\""))
        .map_err(|_| ApiError::Unknown("failed to construct grant precondition".to_owned()))
}

fn grant_precondition(headers: &HeaderMap) -> Result<u64, ApiError> {
    let invalid = || ApiError::BadRequest("invalid authorization precondition".to_owned());
    let mut values = headers.get_all(IF_MATCH).iter();
    let value = values.next().ok_or(ApiError::PreconditionRequired)?;
    if values.next().is_some() {
        return Err(invalid());
    }
    let text = value
        .to_str()
        .map_err(|_| invalid())?
        .trim_matches([' ', '\t']);
    let digits = text
        .strip_prefix("\"auth-")
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(invalid)?;
    if digits.is_empty()
        || digits.len() > 20
        || digits.starts_with('0')
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid());
    }
    digits.parse().map_err(|_| invalid())
}

#[utoipa::path(
    patch,
    path = "/native/{connection_id}",
    params(
        ("connection_id" = String, Path, description = "Canonical connection ID"),
        ("If-Match" = String, Header, description = "Observed Session grant ETag, quoted auth-N")
    ),
    responses(
        (status = 204, description = "Grant replaced; authority observation remains ordered on the control channel", headers(("ETag" = String))),
        (status = 400, description = "Malformed precondition or nonempty body", body = Problem),
        (status = 401, description = "Invalid bearer authorization", body = Problem),
        (status = 403, description = "Credential cannot replace this grant", body = Problem),
        (status = 410, description = "Connection grant is terminal", body = Problem),
        (status = 412, description = "Stale precondition", body = Problem, headers(("ETag" = String))),
        (status = 428, description = "Precondition required", body = Problem),
        (status = 429, description = "Controller is busy", body = Problem),
        (status = 503, description = "Service unavailable", body = Problem)
    ),
    security(("bearer_auth" = [])),
    tag = "native"
)]
async fn patch_native(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let headers = request.headers();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let principal = authenticate_bearer(headers, &state.key_registry, now)?;
    let expected_revision = grant_precondition(headers)?;
    let connection_id = connection_id.parse().map_err(|_| ApiError::NotFound)?;
    to_bytes(request.into_body(), 0).await.map_err(|_| {
        ApiError::BadRequest("native grant replacement requires an empty body".to_owned())
    })?;
    let (send, receive) = pulsebeam_runtime::sync::oneshot::channel();
    state
        .controller
        .try_send(
            (
                controller::NativeResourceRequest {
                    principal,
                    connection_id,
                },
                expected_revision,
                send,
            )
                .into(),
        )
        .map_err(|error| match error {
            TrySendError::Full(_) => ApiError::RateLimited,
            TrySendError::Closed(_) => ApiError::ServiceUnavailable,
        })?;
    let revision: u64 = receive.await.map_err(|_| ApiError::ServiceUnavailable)??;
    Ok((StatusCode::NO_CONTENT, [(ETAG, grant_etag(revision)?)]).into_response())
}

#[utoipa::path(
    delete,
    path = "/native/{connection_id}",
    params(("connection_id" = String, Path, description = "Canonical connection ID")),
    responses(
        (status = 204, description = "Native connection cutoff acknowledged"),
        (status = 401, description = "Invalid bearer authorization", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Credential cannot authorize this resource", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "Malformed connection ID", body = Problem, content_type = "application/problem+json"),
        (status = 410, description = "Resource authorization context is unavailable, not proof of physical absence", body = Problem, content_type = "application/problem+json"),
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
    let principal = authenticate_bearer(&headers, &state.key_registry, now)?;
    let connection_id = connection_id.parse().map_err(|_| ApiError::NotFound)?;
    let (send, receive) = pulsebeam_runtime::sync::oneshot::channel();
    state
        .controller
        .try_send(
            (
                controller::NativeResourceRequest {
                    principal,
                    connection_id,
                },
                send,
            )
                .into(),
        )
        .map_err(|error| match error {
            TrySendError::Full(_) => ApiError::RateLimited,
            TrySendError::Closed(_) => ApiError::ServiceUnavailable,
        })?;
    receive.await.map_err(|_| ApiError::ServiceUnavailable)??;
    Ok(StatusCode::NO_CONTENT)
}

fn requested_sdp_room(uri: &Uri) -> Result<pulsebeam_core::identity::RoomId, ApiError> {
    let invalid = || ApiError::BadRequest("one valid room query parameter is required".to_owned());
    let mut pairs = uri.query().ok_or_else(invalid)?.split('&');
    let (name, encoded) = pairs
        .next()
        .and_then(|pair| pair.split_once('='))
        .ok_or_else(invalid)?;
    if name != "room"
        || pairs.next().is_some()
        || encoded.len() > pulsebeam_core::identity::MAX_APPLICATION_NAME_BYTES.saturating_mul(3)
    {
        return Err(invalid());
    }
    let mut decoded = Vec::with_capacity(pulsebeam_core::identity::MAX_APPLICATION_NAME_BYTES);
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        if decoded.len() == pulsebeam_core::identity::MAX_APPLICATION_NAME_BYTES {
            return Err(invalid());
        }
        decoded.push(match byte {
            b'+' => b' ',
            b'%' => {
                let hex = |byte| {
                    char::from(byte)
                        .to_digit(16)
                        .and_then(|value| u8::try_from(value).ok())
                };
                let high = bytes.next().and_then(hex).ok_or_else(invalid)?;
                let low = bytes.next().and_then(hex).ok_or_else(invalid)?;
                (high << 4) | low
            }
            byte => byte,
        });
    }
    let room = std::str::from_utf8(&decoded).map_err(|_| invalid())?;
    pulsebeam_core::identity::RoomId::new(room).map_err(|_| invalid())
}

async fn create_sdp_profile(
    state: AppState,
    request: Request,
    profile: SignalingProtocol,
    required_direction: &'static str,
    resource: &'static str,
) -> Result<Response, ApiError> {
    let headers = request.headers().clone();
    let wall_now = SystemTime::now();
    let unix_now = wall_now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let principal = authenticate_bearer(&headers, &state.key_registry, unix_now)?;
    let authorization = principal
        .authorize_room(requested_sdp_room(request.uri())?)
        .map_err(|_| controller::ControllerError::AuthorizationRejected)?;
    ensure_sdp_content_type(&headers)?;
    let body = to_bytes(request.into_body(), MAX_SIGNALING_BODY_BYTES)
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?;
    let raw_offer = std::str::from_utf8(&body)
        .map_err(|_| ApiError::BadRequest("SDP body must be UTF-8".to_owned()))?;
    validate_strict_directions(raw_offer, required_direction)?;
    validate_single_media_per_kind(raw_offer)?;
    let offer = raw_offer.to_owned();
    let lease = controller::AuthorizationLease::from_verified(
        authorization.clone(),
        wall_now,
        pulsebeam_runtime::time::Instant::now(),
    )?;
    let connection_id = ConnectionId::new();
    let (reply_tx, reply_rx) = pulsebeam_runtime::sync::oneshot::channel();
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
                        rtp_capabilities: None,
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
    Ok((StatusCode::CREATED, response_headers, reply.answer).into_response())
}

#[utoipa::path(
    post,
    path = "/whip",
    params(("room" = String, Query, description = "Explicit concrete room external ID")),
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
    create_sdp_profile(state, request, SignalingProtocol::Whip, "sendonly", "whip").await
}

#[utoipa::path(
    post,
    path = "/whep",
    params(("room" = String, Query, description = "Explicit concrete room external ID")),
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
    create_sdp_profile(state, request, SignalingProtocol::Whep, "recvonly", "whep").await
}

fn verify_discovery(state: &AppState, headers: &HeaderMap) -> Result<StatusCode, ApiError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let _principal = authenticate_bearer(headers, &state.key_registry, now)?;
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
    profile: SignalingProtocol,
) -> Result<StatusCode, ApiError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let principal = authenticate_bearer(&headers, &state.key_registry, now)?;
    let connection_id = connection_id.parse().map_err(|_| ApiError::NotFound)?;
    state
        .controller
        .try_send(
            controller::DeleteSdpResource {
                principal,
                connection_id,
                profile,
            }
            .into(),
        )
        .map_err(|error| match error {
            TrySendError::Full(_) => ApiError::RateLimited,
            TrySendError::Closed(_) => ApiError::ServiceUnavailable,
        })?;
    Ok(StatusCode::NO_CONTENT)
}

macro_rules! delete_handler {
    ($name:ident, $path:literal, $tag:literal, $profile:expr) => {
        #[utoipa::path(
            delete,
            path = $path,
            params(("connection_id" = String, Path, description = "Canonical connection ID")),
            responses(
                (status = 204, description = "Connection deleted or already absent"),
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
    SignalingProtocol::Whip
);
delete_handler!(
    delete_whep,
    "/whep/{connection_id}",
    "whep",
    SignalingProtocol::Whep
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
        issue_native_admission,
        create_native,
        patch_native,
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
    components(schemas(NativeRequest, NativeResponse, NativeAdmissionRequest, NativeAdmissionResponse, Problem)),
    tags(
        (name = "native", description = "Native signaling resource"),
        (name = "whip", description = "Strict WHIP signaling resource"),
        (name = "whep", description = "Strict WHEP signaling resource")
    )
)]
struct ApiDoc;

pub(crate) fn cors() -> tower_http::cors::CorsLayer {
    tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([
            hyper::Method::GET,
            hyper::Method::HEAD,
            hyper::Method::POST,
            hyper::Method::PATCH,
            hyper::Method::DELETE,
            hyper::Method::OPTIONS,
        ])
        .allow_headers([
            AUTHORIZATION,
            IF_MATCH,
            CONTENT_TYPE,
            hyper::header::CONTENT_ENCODING,
            hyper::header::ACCEPT,
            hyper::header::HeaderName::from_static("pulsebeam-owner"),
            hyper::header::HeaderName::from_static("pulsebeam-attempt"),
        ])
        .expose_headers([LOCATION, ETAG, RETRY_AFTER, ALLOW, ACCEPT_POST])
        .max_age(std::time::Duration::from_secs(86400))
}

pub fn router(controller: ControllerSender, cfg: ApiConfig, key_registry: KeyRegistry) -> Router {
    let openapi = build_openapi(&cfg.base_path);
    let api = Router::new()
        .route("/native", post(create_native))
        .route("/native/admissions", post(issue_native_admission))
        .route(
            "/native/{connection_id}",
            axum::routing::delete(delete_native).patch(patch_native),
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
            key_registry,
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
            DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_PUBLIC_KEY, DEVELOPMENT_PROJECT_ID,
            KeyRegistryEntry,
        },
        identity::{ParticipantId, RoomId},
    };
    use str0m::{
        RtcConfig,
        media::{Direction, MediaKind},
    };
    use tower::ServiceExt;

    fn auth_registry() -> KeyRegistry {
        KeyRegistry::new(
            *DEVELOPMENT_PROJECT_ID,
            vec![KeyRegistryEntry {
                key_id: *DEVELOPMENT_API_KEY_ID,
                public_key: *DEVELOPMENT_API_PUBLIC_KEY,
            }],
        )
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

    fn full_grants(room_pattern: &str) -> pulsebeam_core::auth::grants::GrantSet {
        serde_json::from_value(serde_json::json!([{"rooms":[room_pattern],"allow":["join","publish","discover","subscribe"]}])).unwrap()
    }

    fn live_token() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key().unwrap(),
            &full_grants((RoomId::new("general").unwrap()).as_str()),
            (ParticipantId::new("alice").unwrap()).as_str(),
            now + 60,
        )
        .unwrap()
    }

    fn cache_creation(
        response: creation::CreationResponse<'_>,
        authorization: &VerifiedAuthorization,
        profile: SignalingProtocol,
        offer: &str,
        credential: &str,
    ) -> [controller::ownership::ResultBytes; 2] {
        use controller::ownership::{Admission, Authority, PrincipalKey, RequestBinding};
        use rand::SeedableRng;

        let prepared = response.validate().unwrap();
        let mut authority = Authority::default();
        let authenticated =
            pulsebeam_core::auth::authenticate_participant_token(&auth_registry(), credential, 0)
                .unwrap();
        let principal = PrincipalKey::from_principal(&authenticated);
        assert_eq!(principal, PrincipalKey::from_verified(authorization));
        let now = Instant::now();
        let mut entropy = rand::rngs::StdRng::seed_from_u64(23);
        let proof = authority
            .issue(principal.clone(), now, &mut entropy)
            .unwrap();
        let binding =
            RequestBinding::for_request(profile, &authorization.room_id, offer, credential)
                .unwrap();
        let Admission::Start(ticket) = authority
            .begin(
                &principal,
                proof,
                std::num::NonZeroU64::new(1).unwrap(),
                binding,
                now,
            )
            .unwrap()
        else {
            panic!("expected creation response reservation");
        };
        authority.commit(&ticket, prepared.encode(), now).unwrap();
        [
            authority.result(&ticket, now).unwrap(),
            authority.result(&ticket, now).unwrap(),
        ]
    }

    async fn assert_creation_replay(
        image: controller::ownership::ResultBytes,
        location: &str,
        content_type: &str,
        expected: &[u8],
    ) {
        let budget = creation::DeliveryBudget::new();
        let mut held: Vec<_> = (0..creation::MAX_DELIVERIES)
            .map(|_| creation::Delivery::new(image.clone(), budget.reserve().unwrap()))
            .collect();
        assert!(budget.reserve().is_none());
        let mut cached = Some(creation::into_response(held.pop().unwrap()).unwrap());
        let service = tower::service_fn(move |_: Request| {
            let response = cached.take().unwrap();
            async move { Ok::<_, std::convert::Infallible>(response) }
        });
        let response =
            tower::Layer::layer(&tower_http::compression::CompressionLayer::new(), service)
                .oneshot(
                    Request::builder()
                        .header(hyper::header::ACCEPT_ENCODING, "zstd")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()[LOCATION], location);
        assert_eq!(response.headers()[CONTENT_TYPE], content_type);
        assert_eq!(
            response.headers()[hyper::header::CONTENT_ENCODING],
            "identity"
        );
        let (parts, body) = response.into_parts();
        let payload = to_bytes(Body::new(body), MAX_SIGNALING_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(payload.as_ref(), expected);
        assert!(budget.reserve().is_none());
        drop(parts);
        let alias = payload.clone();
        drop(payload);
        assert!(
            budget.reserve().is_none(),
            "the final body alias retains its slot"
        );
        drop(alias);
        assert!(budget.reserve().is_some());

        let response =
            creation::into_response(creation::Delivery::new(image, budget.reserve().unwrap()))
                .unwrap();
        let location = response.headers()[LOCATION].clone();
        drop(response);
        assert!(
            budget.reserve().is_none(),
            "a retained header also owns the slot"
        );
        drop(location);
        assert!(budget.reserve().is_some());
        drop(held);
    }

    fn native_post(token: &str, body: Vec<u8>) -> Request {
        Request::builder()
            .method("POST")
            .uri("/api/v1/native")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(CONTENT_TYPE, "application/json")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "media.example")
            .header("pulsebeam-owner", "0".repeat(52))
            .header("pulsebeam-attempt", "1")
            .body(Body::from(body))
            .unwrap()
    }

    fn sdp_post(token: &str, profile: &str, body: Vec<u8>) -> Request {
        Request::builder()
            .method("POST")
            .uri(format!("/api/v1/{profile}?room=general"))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(CONTENT_TYPE, "application/sdp")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "media.example")
            .body(Body::from(body))
            .unwrap()
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn native_post_requires_authenticated_ownership_admission() {
        let (controller, mut commands) =
            pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
        let app = router(controller, cfg(), auth_registry());
        let token = live_token();
        let body = serde_json::to_vec(&serde_json::json!({
            "profile": "pulsebeam.media.v1",
            "offer": valid_offer(),
            "room": "general"
        }))
        .unwrap();

        let mut missing_proof = native_post(&token, body.clone());
        missing_proof.headers_mut().remove("pulsebeam-owner");
        let response = app.clone().oneshot(missing_proof).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            commands.try_recv().is_err(),
            "missing ownership proof reached controller"
        );

        let authorization = verify_bearer(
            &authorization_headers(&token),
            &auth_registry(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let offer = valid_offer();
        let (message, capabilities) =
            native_message(&authorization, offer.clone(), SystemTime::now()).unwrap();
        assert!(message.state.manual_sub);
        assert_eq!(message.state.profile, SignalingProtocol::State);
        assert!(message.state.authorization.is_some());
        assert_eq!(message.state.rtp_capabilities, Some(capabilities.clone()));
        assert_eq!(message.offer, offer);
        let response_body = serde_json::to_value(NativeResponse::new(
            &authorization,
            message.state.connection_id,
            &capabilities,
            "v=0\r\n",
        ))
        .unwrap();
        assert_eq!(response_body["room_id"], authorization.room_id.as_str());
        assert!(response_body.get("room_external_id").is_none());
        let long_location = HeaderValue::from_bytes(&vec![b'a'; 300_000]).unwrap();
        let escaped = "\0".repeat(MAX_NATIVE_SDP_BYTES);
        assert!(matches!(
            creation::CreationResponse::native(
                &long_location,
                &authorization,
                message.state.connection_id,
                &capabilities,
                &escaped,
            )
            .validate(),
            Err(creation::Error::TooLarge)
        ));
        assert!(
            creation::CreationResponse::sdp(&long_location, &escaped)
                .validate()
                .is_ok()
        );

        let mut invalid = token;
        invalid.push('x');
        let response = app.oneshot(native_post(&invalid, body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            commands.try_recv().is_err(),
            "invalid bearer reached controller"
        );
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn malformed_offer_is_a_negotiation_error_response() {
        let (controller, mut commands) =
            pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
        let app = router(controller, cfg(), auth_registry());
        let offer = "not SDP\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n";
        let body = offer.as_bytes().to_vec();
        let responder = pulsebeam_runtime::task::spawn(async move {
            let controller::ControllerCommand::CreateParticipant(message, reply) =
                commands.recv().await.unwrap()
            else {
                panic!("expected create command")
            };
            assert_eq!(message.offer, offer);
            let mut negotiator = crate::control::negotiator::Negotiator::new(vec![
                pulsebeam_rtc::LocalCandidate::Udp("127.0.0.1:6000".parse().unwrap()),
            ]);
            let result = negotiator.create_answer(
                message.offer,
                crate::control::ufrag::test_credentials(),
                pulsebeam_runtime::net::ReceiveTime::now(),
                None,
            );
            let Err(error @ crate::control::negotiator::NegotiatorError::InvalidOffer(_)) = result
            else {
                panic!("malformed SDP must fail at admission");
            };
            reply.send(Err(error.into())).unwrap();
        });
        let response = app
            .oneshot(sdp_post(&live_token(), "whip", body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["status"], 400);
        assert_eq!(problem["title"], "Bad Request");
        assert!(
            problem["detail"]
                .as_str()
                .unwrap()
                .contains("invalid SDP offer")
        );
        responder.await.unwrap();
    }

    #[test]
    fn sdp_room_query_accepts_case_preserving_names_and_decodes_once() {
        let uri: Uri = "/whip?room=%53tage_9-A".parse().unwrap();
        assert_eq!(requested_sdp_room(&uri).unwrap().as_str(), "Stage_9-A");
        for invalid in ["stage.main", "stage%2Fmain", "stage%252Fmain", "caf%C3%A9"] {
            let uri: Uri = format!("/whip?room={invalid}").parse().unwrap();
            assert!(requested_sdp_room(&uri).is_err());
        }
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn native_post_rejects_malformed_contract_before_controller_allocation() {
        let token = live_token();
        let cases = [
            serde_json::to_vec(&serde_json::json!({"profile": "unsupported", "offer": valid_offer(), "room":"general"})).unwrap(),
            serde_json::to_vec(&serde_json::json!({"profile": "pulsebeam.media.v1", "offer": valid_offer(), "room":"general", "unknown": true}))
                .unwrap(),
            b"{".to_vec(),
            vec![b' '; MAX_NATIVE_BODY_BYTES + 1],
            serde_json::to_vec(&serde_json::json!({"profile": "pulsebeam.media.v1", "offer": " ".repeat(MAX_NATIVE_SDP_BYTES + 1), "room":"general"})).unwrap(),
            br#"{"offer":"v=0","offer":"v=1"}"#.to_vec(),
            br#"{"profile":"pulsebeam.media.v1","offer":"v=0","room":""}"#.to_vec(),
            br#"{"profile":"pulsebeam.media.v1","offer":"v=0","room":"a","room":"b"}"#.to_vec(),
            serde_json::to_vec(&serde_json::json!({"profile":"pulsebeam.media.v1","offer":"v=0","room":"é".repeat(129)})).unwrap(),
            serde_json::to_vec(&serde_json::json!({"profile": "pulsebeam.media.v1", "offer": valid_offer(), "room":"general", ("diagnostic-secret-".repeat(32)): true})).unwrap(),
        ];
        for body in cases {
            let (controller, mut commands) =
                pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
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
            let body = to_bytes(response.into_body(), 512).await.unwrap();
            assert!(
                !String::from_utf8(body.to_vec())
                    .unwrap()
                    .contains("diagnostic-secret")
            );
        }

        let (controller, _) =
            pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
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

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn native_selected_room_rejects_before_controller_allocation() {
        let (controller, mut commands) =
            pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
        let body = serde_json::to_vec(&serde_json::json!({"profile":"pulsebeam.media.v1","offer":valid_offer(),"room":"other"})).unwrap();
        let response = router(controller.clone(), cfg(), auth_registry())
            .oneshot(native_post(&live_token(), body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(commands.try_recv().is_err());

        let app = owned_native_router(controller, cfg(), auth_registry());
        let token = live_token();
        let offer = valid_offer();
        let create = |room: Option<&str>| {
            let mut request = native_post(
                &token,
                serde_json::to_vec(&serde_json::json!({
                    "profile": pulsebeam_proto::state::PROFILE, "offer": offer, "room": room,
                }))
                .unwrap(),
            );
            request.headers_mut().insert(
                "pulsebeam-owner",
                HeaderValue::from_str(&"0".repeat(52)).unwrap(),
            );
            request
                .headers_mut()
                .insert("pulsebeam-attempt", HeaderValue::from_static("1"));
            request
        };
        let mut missing_proof = create(Some("general"));
        missing_proof.headers_mut().remove("pulsebeam-owner");
        let mut no_identity = create(Some("general"));
        no_identity.headers_mut().insert(
            hyper::header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity;q=0, zstd"),
        );
        let mut bad_location = create(Some("general"));
        bad_location.headers_mut().insert(
            "x-forwarded-proto",
            HeaderValue::from_static("not a scheme"),
        );
        let unrepresentable_token = pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key().unwrap(),
            &full_grants("general"),
            "alice",
            u64::MAX,
        )
        .unwrap();
        let mut unrepresentable = create(Some("general"));
        unrepresentable.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {unrepresentable_token}")).unwrap(),
        );
        let mut rejected = vec![
            (unrepresentable, StatusCode::UNAUTHORIZED),
            (create(None), StatusCode::BAD_REQUEST),
            (create(Some("other")), StatusCode::FORBIDDEN),
            (missing_proof, StatusCode::BAD_REQUEST),
            (no_identity, StatusCode::NOT_ACCEPTABLE),
            (bad_location, StatusCode::INTERNAL_SERVER_ERROR),
        ];
        for body in [
            b"[]".to_vec(),
            br#"{"room":"general"}"#.to_vec(),
            vec![b' '; 1_025],
        ] {
            let status = if body.len() > 1_024 {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            rejected.push((
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/native/admissions")
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
                status,
            ));
        }
        for (request, status) in rejected {
            let response = pulsebeam_runtime::select! {
                response = app.clone().oneshot(request) => response.unwrap(),
                _ = commands.recv() => panic!("rejected ownership preflight reached the controller"),
            };
            assert_eq!(response.status(), status);
            assert!(commands.try_recv().is_err());
        }
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn native_patch_requires_exact_precondition_and_empty_body() {
        let (controller, mut commands) =
            pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
        let app = router(controller, cfg(), auth_registry()).layer(cors());
        let token = live_token();
        let connection = ConnectionId::new();
        for (tag, expected) in [
            (None, StatusCode::PRECONDITION_REQUIRED),
            (Some("*"), StatusCode::BAD_REQUEST),
            (Some("W/\"auth-1\""), StatusCode::BAD_REQUEST),
            (Some("\"auth-0\""), StatusCode::BAD_REQUEST),
            (Some("\"auth-01\""), StatusCode::BAD_REQUEST),
            (Some("\"auth-+1\""), StatusCode::BAD_REQUEST),
            (
                Some("\"auth-18446744073709551616\""),
                StatusCode::BAD_REQUEST,
            ),
            (Some("\"auth-1\", \"auth-2\""), StatusCode::BAD_REQUEST),
        ] {
            let mut request = Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/native/{connection}"))
                .header(AUTHORIZATION, format!("Bearer {token}"));
            if let Some(tag) = tag {
                request = request.header(IF_MATCH, tag);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{tag:?}");
        }
        for (duplicate, body) in [
            (true, Body::empty()),
            (false, Body::from("not-a-grant-body")),
        ] {
            let mut request = Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/native/{connection}"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header(IF_MATCH, "\"auth-1\"");
            if duplicate {
                request = request.header(IF_MATCH, "\"auth-1\"");
            }
            let response = app
                .clone()
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        assert!(commands.try_recv().is_err());
        let observed = authenticate_bearer(
            &authorization_headers(&token),
            &auth_registry(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let responder = pulsebeam_runtime::task::spawn(async move {
            for result in [
                Ok(2),
                Err(controller::ControllerError::AuthorizationPrecondition { revision: 2 }),
                Err(controller::ControllerError::GrantGone),
            ] {
                let controller::ControllerCommand::ReplaceAuthorization(
                    message,
                    expected_revision,
                    reply,
                ) = commands.recv().await.unwrap()
                else {
                    panic!("expected grant replacement")
                };
                assert_eq!(message.connection_id, connection);
                assert_eq!(expected_revision, 1);
                assert_eq!(message.principal.project_id(), observed.project_id());
                assert_eq!(
                    message.principal.participant_id(),
                    observed.participant_id()
                );
                assert_eq!(message.principal.expiry(), observed.expiry());
                assert_eq!(format!("{message:?}"), "NativeResourceRequest([redacted])");
                reply.send(result).unwrap();
            }
        });
        for (status, tag) in [
            (StatusCode::NO_CONTENT, Some("\"auth-2\"")),
            (StatusCode::PRECONDITION_FAILED, Some("\"auth-2\"")),
            (StatusCode::GONE, None),
        ] {
            let request = Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/native/{connection}"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header(IF_MATCH, "\"auth-1\"")
                .header("origin", "https://app.example")
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), status);
            assert!(
                response.headers()["access-control-expose-headers"]
                    .to_str()
                    .unwrap()
                    .split(',')
                    .any(|header| header.trim().eq_ignore_ascii_case("etag"))
            );
            assert_eq!(
                response.headers().get(ETAG).map(|v| v.to_str().unwrap()),
                tag
            );
            let body = to_bytes(response.into_body(), MAX_NATIVE_BODY_BYTES)
                .await
                .unwrap();
            assert!(!String::from_utf8_lossy(&body).contains(&token));
        }
        responder.await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(IF_MATCH, grant_etag(u64::MAX).unwrap());
        assert_eq!(grant_precondition(&headers).unwrap(), u64::MAX);
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn native_delete_uses_claim_identity_profile_and_path_connection() {
        let (controller, mut commands) =
            pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
        let token = live_token();
        let connection_id = ConnectionId::new();
        let request = Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/native/{connection_id}"))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let app = router(controller, cfg(), auth_registry());
        let response = app.oneshot(request);
        pulsebeam_runtime::pin!(response);
        assert!(
            futures_lite::future::poll_once(&mut response)
                .await
                .is_none()
        );
        let controller::ControllerCommand::DeleteNative(message, reply) =
            commands.recv().await.unwrap()
        else {
            panic!("expected native delete command")
        };
        assert_eq!(message.connection_id, connection_id);
        assert_eq!(message.principal.project_id(), *DEVELOPMENT_PROJECT_ID);
        assert_eq!(message.principal.participant_id().as_str(), "alice");
        assert_eq!(format!("{message:?}"), "NativeResourceRequest([redacted])");
        assert!(
            futures_lite::future::poll_once(&mut response)
                .await
                .is_none()
        );
        reply.send(Ok(())).unwrap();
        assert_eq!(response.await.unwrap().status(), StatusCode::NO_CONTENT);

        let (controller, _) =
            pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
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

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn strict_sdp_posts_return_raw_answers_and_opaque_locations() {
        const ROOM: &str = "RoomMetricSentinel";
        const PARTICIPANT: &str = "ParticipantMetricSentinel";
        for (resource, direction, profile) in [
            ("whip", Direction::SendOnly, SignalingProtocol::Whip),
            ("whep", Direction::RecvOnly, SignalingProtocol::Whep),
        ] {
            let (controller, mut commands) =
                pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
            let app = router(controller, cfg(), auth_registry());
            let token = pulsebeam_server::sign_participant_token(
                &pulsebeam_server::insecure_dev_api_key().unwrap(),
                &full_grants(ROOM),
                PARTICIPANT,
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 60,
            )
            .unwrap();
            let offer = directional_offer(direction);
            let answer = offer.clone();
            let captured_token = token.clone();
            let responder = pulsebeam_runtime::task::spawn(async move {
                let controller::ControllerCommand::CreateParticipant(message, reply) =
                    commands.recv().await.unwrap()
                else {
                    panic!("expected create command")
                };
                assert_eq!(message.state.profile, profile);
                assert_eq!(message.state.room_id.as_str(), ROOM);
                assert_eq!(message.state.participant_id.as_str(), PARTICIPANT);
                assert!(!message.state.manual_sub);
                let authorization = authenticate_bearer(
                    &authorization_headers(&captured_token),
                    &auth_registry(),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                )
                .unwrap()
                .authorize_room(message.state.room_id)
                .unwrap();
                let location = HeaderValue::from_str(&format!(
                    "https://media.example/api/v1/{resource}/{}",
                    message.state.connection_id
                ))
                .unwrap();
                let oversized = "a".repeat(controller::ownership::MAX_RESULT_BYTES);
                assert!(matches!(
                    creation::CreationResponse::sdp(&location, &oversized).validate(),
                    Err(creation::Error::TooLarge)
                ));
                let images = cache_creation(
                    creation::CreationResponse::sdp(&location, &answer),
                    &authorization,
                    profile,
                    &message.offer,
                    &captured_token,
                );
                reply.send(Ok(CreateParticipantReply { answer })).unwrap();
                images
            });
            let mut request = sdp_post(&token, resource, offer.into_bytes());
            *request.uri_mut() = format!("/api/v1/{resource}?room={ROOM}").parse().unwrap();
            let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
            let mut response = std::pin::pin!(app.oneshot(request));
            let response = std::future::poll_fn(|cx| {
                metrics::with_local_recorder(&recorder, || response.as_mut().poll(cx))
            })
            .await
            .unwrap();
            let metrics = recorder.handle().render();
            assert!(metrics.contains("http_request_duration_seconds"));
            assert!(metrics.contains(&format!("path=\"/api/v1/{resource}\"")));
            assert!(!metrics.contains(ROOM));
            assert!(!metrics.contains(PARTICIPANT));
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(response.headers()[CONTENT_TYPE], "application/sdp");
            assert!(!response.headers().contains_key("etag"));
            assert!(!response.headers().contains_key("pb-participant-id"));
            let location = response.headers()[LOCATION].to_str().unwrap().to_owned();
            let connection_id = location
                .strip_prefix(&format!("https://media.example/api/v1/{resource}/"))
                .unwrap()
                .parse::<ConnectionId>()
                .unwrap();
            assert!(connection_id.as_str().starts_with("conn_0"));
            let body = to_bytes(response.into_body(), MAX_SIGNALING_BODY_BYTES)
                .await
                .unwrap();
            assert!(body.starts_with(b"v=0"));
            for image in responder.await.unwrap() {
                assert_creation_replay(image, &location, "application/sdp", &body).await;
            }
        }
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
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
            let (controller, mut commands) =
                pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
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

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn whip_and_whep_delete_use_claim_identity_path_and_profile() {
        for (resource, profile) in [
            ("whip", SignalingProtocol::Whip),
            ("whep", SignalingProtocol::Whep),
        ] {
            let (controller, mut commands) =
                pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
            let token = live_token();
            let connection_id = ConnectionId::new();
            let request = Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/{resource}/{connection_id}?room=general"))
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let response = router(controller, cfg(), auth_registry())
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            let controller::ControllerCommand::DeleteSdpResource(message) =
                commands.recv().await.unwrap()
            else {
                panic!("expected SDP resource delete command")
            };
            assert_eq!(message.connection_id, connection_id);
            assert_eq!(message.profile, profile);
            let expected = authenticate_participant_token(&auth_registry(), &token, 0).unwrap();
            assert_eq!(message.principal, expected);
        }

        let token = live_token();
        for resource in ["whip", "whep"] {
            let (controller, _) =
                pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
            let malformed = Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/{resource}/not-a-connection?room=general"))
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
    fn strict_request_requires_explicit_room_and_unique_known_fields() {
        let selected = parse_native_request(
            br#"{"profile":"pulsebeam.media.v1","offer":"v=0","room":"other"}"#,
        )
        .unwrap();
        assert_eq!(selected.profile, pulsebeam_proto::state::PROFILE);
        assert_eq!(selected.room, "other");
        for body in [
            br#"{"offer":"v=0"}"#.as_slice(),
            br#"{"profile":"pulsebeam.media.v1","offer":"v=0"}"#,
            br#"["pulsebeam.media.v1","v=0","other"]"#,
            br#"{"profile":"pulsebeam.media.v1","offer":"v=0","offer":"v=1","room":"other"}"#,
            br#"{"profile":"pulsebeam.media.v1","offer":"v=0","room":"other","r\u006fom":"another"}"#,
            br#"{"profile":"old","offer":"v=0"}"#,
            br#"{"profile":null,"offer":"v=0"}"#,
            br#"{"profile":"pulsebeam.media.v1","offer":"v=0","room":[]}"#,
            br#"{"profile":"pulsebeam.media.v1","offer":"v=0","manual":true}"#,
            br#"{"profile":"pulsebeam.media.v1","offer":"v=0","rtp_capabilities":{}}"#,
        ] {
            assert!(parse_native_request(body).is_err());
        }
    }

    #[test]
    fn internal_rtc_initialization_is_not_an_offer_rejection() {
        for error in [
            pulsebeam_rtc::AcceptError::InvalidConfiguration,
            pulsebeam_rtc::AcceptError::CryptographicFailure,
        ] {
            let response = ApiError::JoinError(controller::ControllerError::OfferRejected(
                crate::control::negotiator::NegotiatorError::Initialization(error),
            ))
            .into_response();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        let response = ApiError::JoinError(controller::ControllerError::OfferRejected(
            crate::control::negotiator::NegotiatorError::Rtc(
                pulsebeam_rtc::AcceptError::MissingPacketFeedback,
            ),
        ))
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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
    fn whip_and_whep_reject_multiple_active_tracks_of_one_kind() {
        let one_of_each = "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                           m=video 9 UDP/TLS/RTP/SAVPF 96\r\n";
        assert!(validate_single_media_per_kind(one_of_each).is_ok());
        let duplicate_audio = format!("{one_of_each}m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n");
        assert!(validate_single_media_per_kind(&duplicate_audio).is_err());
        let duplicate_video = format!("{one_of_each}m=video 9 UDP/TLS/RTP/SAVPF 96\r\n");
        assert!(validate_single_media_per_kind(&duplicate_video).is_err());
        let rejected_duplicate = format!("{one_of_each}m=audio 0 UDP/TLS/RTP/SAVPF 111\r\n");
        assert!(validate_single_media_per_kind(&rejected_duplicate).is_ok());
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

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn authorization_failures_are_secret_safe_rfc_9457_responses() {
        let token = pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key().unwrap(),
            &full_grants((RoomId::new("general").unwrap()).as_str()),
            (ParticipantId::new("alice").unwrap()).as_str(),
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

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn strict_profile_discovery_has_exact_methods_and_authentication() {
        let (controller, _) =
            pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
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
                "/native/admissions",
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
        assert!(document["paths"]["/native/{connection_id}"]["patch"].is_object());
        assert!(document["paths"]["/native/{connection_id}"]["patch"]["responses"]["412"]["headers"]["ETag"].is_object());
        for profile in ["whip", "whep"] {
            assert!(document["paths"][format!("/{profile}/{{connection_id}}")]["patch"].is_null());
        }
        for old in ["/rooms", "pb-participant-id"] {
            assert!(
                !json.contains(old),
                "old contract remained in OpenAPI: {old}"
            );
        }
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn router_serves_embedded_swagger_assets() {
        let (controller, _) =
            pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
        let app = router(controller, cfg(), auth_registry());
        let redirect = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/swagger-ui")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(redirect.status(), StatusCode::SEE_OTHER);
        assert_eq!(redirect.headers()[LOCATION], "/swagger-ui/");

        for (uri, content_type) in [
            ("/swagger-ui/", "text/html"),
            ("/swagger-ui/swagger-ui.css", "text/css"),
            ("/api-docs/openapi.json", "application/json"),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert!(
                response.headers()[CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with(content_type)
            );
            let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .unwrap();
            assert!(!body.is_empty(), "{uri}");
            if uri == "/api-docs/openapi.json" {
                let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert!(document["paths"]["/native"].is_object());
            }
        }
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn router_exposes_native_without_legacy_signaling_routes() {
        let (controller, _) =
            pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
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

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn native_options_remains_available_through_cors() {
        let (controller, _) =
            pulsebeam_queue::bounded(const { core::num::NonZeroUsize::new(1).unwrap() });
        let app = router(controller, cfg(), auth_registry()).layer(cors());
        for (method, path) in [
            ("POST", "/api/v1/native"),
            ("PATCH", "/api/v1/native/opaque"),
        ] {
            let request = Request::builder()
                .method("OPTIONS")
                .uri(path)
                .header("origin", "https://app.example")
                .header("access-control-request-method", method)
                .header("access-control-request-headers", "authorization,if-match")
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                response.headers()["access-control-allow-methods"]
                    .to_str()
                    .unwrap()
                    .split(',')
                    .any(|value| value.trim() == method)
            );
            assert!(
                response.headers()["access-control-allow-headers"]
                    .to_str()
                    .unwrap()
                    .split(',')
                    .any(|value| value.trim().eq_ignore_ascii_case("if-match"))
            );
        }
    }

    #[test]
    fn admission_failures_have_stable_statuses() {
        use controller::{OwnedAdmissionError, ownership::Error};
        assert_eq!(
            ApiError::JoinError(controller::ControllerError::Superseded)
                .into_response()
                .status(),
            StatusCode::CONFLICT
        );
        for (error, status) in [
            (Error::InvalidProof, StatusCode::BAD_REQUEST),
            (Error::UnknownProof, StatusCode::GONE),
            (Error::Replaced, StatusCode::CONFLICT),
            (Error::StaleAttempt, StatusCode::CONFLICT),
            (Error::ChangedAttempt, StatusCode::BAD_REQUEST),
            (Error::Capacity, StatusCode::SERVICE_UNAVAILABLE),
            (Error::ReplayUnavailable, StatusCode::GONE),
            (Error::QuiescencePending, StatusCode::SERVICE_UNAVAILABLE),
            (Error::ResultTooLarge, StatusCode::PAYLOAD_TOO_LARGE),
            (Error::InvalidTransition, StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            let response =
                ProblemResponse::from(OwnedAdmissionError::Authority(error)).into_response();
            assert_eq!(response.status(), status);
            assert_eq!(
                response.headers().contains_key(RETRY_AFTER),
                status == StatusCode::SERVICE_UNAVAILABLE
            );
            assert!(!response.headers().contains_key(WWW_AUTHENTICATE));
        }
        for (error, status) in [
            (creation::Error::TooLarge, StatusCode::PAYLOAD_TOO_LARGE),
            (
                creation::Error::Serialization,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                creation::Error::InvalidImage,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(
                ProblemResponse::from(OwnedAdmissionError::Image(error)).status(),
                status
            );
        }
    }
}
