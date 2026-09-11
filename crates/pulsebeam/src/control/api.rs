use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{MatchedPath, Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, LOCATION, WWW_AUTHENTICATE};
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
        self, ConnectionProfile, ControllerHandle, CreateParticipantReply, ParticipantState,
    },
    entity::{ConnectionId, IdValidationError, ParticipantId, RoomId},
};

const MAX_NATIVE_BODY_BYTES: usize = 1_048_576;

#[derive(Clone)]
struct AppState {
    controller: ControllerHandle,
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

fn validate_native_directions(offer: &str) -> Result<(), ApiError> {
    let mut active_rtp = false;
    let mut session_direction = None;
    let mut current_active_rtp = false;
    let mut current_direction = None;
    let mut in_media = false;
    let finish = |active: bool, direction: Option<&str>| -> Result<(), ApiError> {
        if active && !matches!(direction, Some("sendonly" | "recvonly")) {
            return Err(ApiError::BadRequest(
                "active RTP media sections must be sendonly or recvonly".to_owned(),
            ));
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
            current_active_rtp = port != Some("0")
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
    if !active_rtp {
        return Err(ApiError::BadRequest(
            "offer must contain an active RTP media section".to_owned(),
        ));
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
    let body = to_bytes(request.into_body(), MAX_NATIVE_BODY_BYTES)
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?;
    let request: NativeRequest = serde_json::from_slice(&body)
        .map_err(|error| ApiError::BadRequest(format!("invalid JSON request: {error}")))?;
    if request.offer.len() > MAX_NATIVE_BODY_BYTES {
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
    paths(create_native, delete_native),
    components(schemas(NativeRequest, NativeResponse, Problem)),
    tags((name = "native", description = "Native signaling resource"))
)]
struct ApiDoc;

pub fn router(
    controller: ControllerHandle,
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
        let body = to_bytes(response.into_body(), MAX_NATIVE_BODY_BYTES)
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
            vec![b' '; MAX_NATIVE_BODY_BYTES + 1],
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

    #[test]
    fn openapi_contains_only_native_signaling_routes() {
        let paths: Vec<_> = build_openapi("/api/v1")
            .paths
            .paths
            .keys()
            .cloned()
            .collect();
        assert_eq!(paths, ["/native", "/native/{connection_id}"]);
        let json = serde_json::to_string(&build_openapi("/api/v1")).unwrap();
        for old in ["/rooms", "pb-participant-id", "If-Match", "ETag", "patch"] {
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
