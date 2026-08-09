use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use hyper::header::{AUTHORIZATION, ETAG, IF_MATCH, LOCATION, WWW_AUTHENTICATE};
use serde::{Deserialize, Serialize};
use str0m::change::SdpOffer;
use utoipa::ToSchema;

use crate::{
    control::{
        api::{ApiConfig, ApiError, AppState, JoinKind, join_core},
        auth::{AccessClaims, AuthError, Capabilities},
        controller::{self, ControllerError},
    },
    entity::{ConnectionEpoch, ConnectionId, ExternalRoomId, Identity, ParticipantId},
};
use pulsebeam_runtime::mailbox::TrySendError;
use pulsebeam_runtime::rand::os_rng;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientInfo {
    #[serde(default)]
    pub sdk: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JoinRequest {
    pub sdp: String,
    #[serde(default)]
    pub manual_sub: bool,
    #[serde(default)]
    pub client: Option<ClientInfo>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResumeRequest {
    pub sdp: String,
    pub resume_token: String,
    #[serde(default)]
    pub manual_sub: bool,
    #[serde(default)]
    pub client: Option<ClientInfo>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IdentityView {
    #[schema(value_type = String)]
    pub subject: Identity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionResponse {
    /// SDP answer.
    pub sdp: String,
    #[schema(value_type = String)]
    pub room: ExternalRoomId,
    #[schema(value_type = String)]
    pub participant_id: ParticipantId,
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    #[schema(value_type = u32)]
    pub epoch: ConnectionEpoch,
    /// Absolute URL; identical to the `Location` header.
    pub resource: String,
    pub resume_token: String,
    pub resume_expires_at: i64,
    pub session_expires_at: i64,
    pub identity: IdentityView,
    pub capabilities: Capabilities,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Stable machine-readable code; never localized, never reworded.
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u32>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

/// Renders an [`ApiError`] as the JSON envelope instead of the legacy `text/plain` body.
pub struct JsonApiError(pub ApiError);

impl From<ApiError> for JsonApiError {
    fn from(e: ApiError) -> Self {
        Self(e)
    }
}

impl From<AuthError> for JsonApiError {
    fn from(e: AuthError) -> Self {
        Self(ApiError::from(e))
    }
}

impl From<crate::entity::IdValidationError> for JsonApiError {
    fn from(e: crate::entity::IdValidationError) -> Self {
        Self(ApiError::from(e))
    }
}

impl IntoResponse for JsonApiError {
    fn into_response(self) -> Response {
        let status = self.0.status();
        let code = self.0.code();
        let mut headers = HeaderMap::new();
        if status == StatusCode::UNAUTHORIZED {
            let challenge = format!("Bearer error=\"invalid_token\", error_description=\"{code}\"");
            if let Ok(value) = challenge.parse() {
                headers.insert(WWW_AUTHENTICATE, value);
            }
        }
        let body = ErrorResponse {
            error: ErrorBody {
                code,
                message: self.0.to_string(),
                retry_after_ms: (status == StatusCode::TOO_MANY_REQUESTS).then_some(250),
            },
        };
        (status, headers, axum::Json(body)).into_response()
    }
}

/// Unix seconds. The JSON layer is the only place that reads a wall clock; everything below it
/// takes the timestamp as a parameter so the simulator's virtual time governs.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn bearer(headers: &HeaderMap) -> Result<&str, AuthError> {
    let value = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::MissingToken)?;
    value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .ok_or(AuthError::MalformedToken)
}

/// Verifies the bearer token and binds it to the room in the path.
pub(crate) fn authorize(
    cfg: &ApiConfig,
    headers: &HeaderMap,
    room: &ExternalRoomId,
    now: i64,
) -> Result<AccessClaims, AuthError> {
    let auth = cfg.auth.as_ref().ok_or(AuthError::NotConfigured)?;
    auth.verify_access(bearer(headers)?, room, now)
}

fn parse_body<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::InvalidJson(e.to_string()))
}

/// The offer must not ask for anything the token withholds.
///
/// Directions in an offer are from the offerer's point of view: a client `sendonly` line is a
/// publish, a `recvonly` line is a subscribe.
fn check_capabilities(offer: &SdpOffer, caps: Capabilities) -> Result<(), ApiError> {
    use str0m::media::Direction;
    for media in &offer.media_lines {
        if media.typ.to_string() == "application" {
            continue;
        }
        let publishes = matches!(media.direction(), Direction::SendOnly | Direction::SendRecv);
        let subscribes = matches!(media.direction(), Direction::RecvOnly | Direction::SendRecv);
        if publishes && !caps.publish {
            return Err(ApiError::Forbidden("publish_denied"));
        }
        if subscribes && !caps.subscribe {
            return Err(ApiError::Forbidden("subscribe_denied"));
        }
    }
    Ok(())
}

fn session_response(
    s: &AppState,
    claims: &AccessClaims,
    room: &ExternalRoomId,
    outcome: crate::control::api::JoinOutcome,
    epoch: ConnectionEpoch,
    now: i64,
) -> Result<SessionResponse, ApiError> {
    let auth = s.api_config.auth.as_ref().ok_or(AuthError::NotConfigured)?;
    let (resume_token, resume_expires_at) = auth.mint_resume(
        claims,
        room,
        outcome.state.participant_id,
        epoch,
        now,
        &mut os_rng(),
    )?;

    Ok(SessionResponse {
        sdp: outcome.answer.to_sdp_string(),
        room: room.clone(),
        participant_id: outcome.state.participant_id,
        connection_id: outcome.state.connection_id,
        epoch,
        resource: outcome.location,
        resume_token,
        resume_expires_at,
        session_expires_at: claims.session_expires_at(now),
        identity: IdentityView {
            subject: claims.sub.clone(),
            name: claims.pb.name.clone(),
        },
        capabilities: claims.capabilities(),
    })
}

fn header_map(location: &str, connection_id: &ConnectionId) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = location.parse() {
        headers.insert(LOCATION, value);
    }
    if let Ok(value) = connection_id.as_str().parse() {
        headers.insert(ETAG, value);
    }
    headers
}

/// Join a room.
#[utoipa::path(
    post,
    path = "/rooms/{external_room_id}/participants",
    request_body(content = JoinRequest, content_type = "application/json"),
    params(("external_room_id" = String, Path, description = "External room identifier")),
    responses(
        (status = 201, description = "Joined", body = SessionResponse, content_type = "application/json"),
        (status = 400, description = "Malformed request", body = ErrorResponse, content_type = "application/json"),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse, content_type = "application/json"),
        (status = 403, description = "Token does not authorize this room", body = ErrorResponse, content_type = "application/json"),
        (status = 503, description = "Unavailable or auth not configured", body = ErrorResponse, content_type = "application/json"),
    ),
    tag = "participants-json"
)]
pub(crate) async fn join(
    s: AppState,
    external_room_id: ExternalRoomId,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<Response, JsonApiError> {
    let now = now_secs();
    let claims = authorize(&s.api_config, &headers, &external_room_id, now)?;

    let req: JoinRequest = parse_body(&body)?;
    let offer = SdpOffer::from_sdp_string(&req.sdp).map_err(ApiError::OfferInvalid)?;
    check_capabilities(&offer, claims.capabilities())?;

    let outcome = join_core(
        &s,
        &headers,
        JoinKind::Create,
        &external_room_id,
        req.manual_sub,
        offer,
    )
    .await?;

    let headers_out = header_map(&outcome.location, &outcome.state.connection_id);
    let connection_id = outcome.state.connection_id;
    let body = session_response(
        &s,
        &claims,
        &external_room_id,
        outcome,
        ConnectionEpoch::ZERO,
        now,
    )?;

    debug_assert_eq!(body.connection_id, connection_id);
    Ok((StatusCode::CREATED, headers_out, axum::Json(body)).into_response())
}

/// Resume or reconstruct a participant.
///
/// Idempotent create-or-replace at a client-known URI: it behaves the same whether the
/// participant is live, gone, or its node has restarted.
#[utoipa::path(
    put,
    path = "/rooms/{external_room_id}/participants/{participant_id}",
    request_body(content = ResumeRequest, content_type = "application/json"),
    params(
        ("external_room_id" = String, Path, description = "External room identifier"),
        ("participant_id" = String, Path, description = "Participant identifier"),
    ),
    responses(
        (status = 200, description = "Replaced a live participant", body = SessionResponse, content_type = "application/json"),
        (status = 201, description = "Reconstructed a participant that was no longer live", body = SessionResponse, content_type = "application/json"),
        (status = 401, description = "Invalid access or resume token", body = ErrorResponse, content_type = "application/json"),
        (status = 403, description = "Token does not own this participant", body = ErrorResponse, content_type = "application/json"),
        (status = 415, description = "Not application/json", body = ErrorResponse, content_type = "application/json"),
    ),
    tag = "participants-json"
)]
pub(crate) async fn resume(
    Path((external_room_id, participant_id)): Path<(ExternalRoomId, ParticipantId)>,
    State(s): State<AppState>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<Response, JsonApiError> {
    require_json(&headers)?;
    let now = now_secs();
    // The access token is re-verified on every resume, so expiry and revocation are decided by
    // the application's current answer rather than a snapshot taken at join time.
    let claims = authorize(&s.api_config, &headers, &external_room_id, now)?;

    let req: ResumeRequest = parse_body(&body)?;
    let offer = SdpOffer::from_sdp_string(&req.sdp).map_err(ApiError::OfferInvalid)?;
    check_capabilities(&offer, claims.capabilities())?;

    let auth = s
        .api_config
        .auth
        .as_ref()
        .ok_or(AuthError::NotConfigured)?
        .clone();
    let resume_claims = auth.verify_resume(
        &req.resume_token,
        &external_room_id,
        &participant_id,
        &claims,
        now,
    )?;

    // One past the token's generation: after a restart this is the only surviving memory of how
    // far the session had got.
    let floor = resume_claims
        .epoch
        .checked_next()
        .unwrap_or(resume_claims.epoch);

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let state = controller::ParticipantState {
        manual_sub: req.manual_sub,
        room_id: crate::entity::RoomId::from_external(&external_room_id),
        participant_id,
        connection_id: ConnectionId::new(&mut os_rng()),
        old_connection_id: None,
        epoch: floor,
    };
    let connection_id = state.connection_id;

    s.controller
        .try_send(
            (
                controller::ResumeParticipant {
                    state: state.clone(),
                    offer,
                },
                reply_tx,
            )
                .into(),
        )
        .map_err(|e| match e {
            TrySendError::Full(_) => ApiError::RateLimited,
            TrySendError::Closed(_) => ApiError::ServiceUnavailable,
        })?;

    let reply = reply_rx
        .await
        .map_err(|_| ApiError::JoinError(ControllerError::ServiceUnavailable))?
        .map_err(ApiError::JoinError)?;

    let path = format!(
        "/rooms/{}/participants/{}",
        &external_room_id, &participant_id
    );
    let location =
        crate::control::api::build_location_for(&headers, &s.api_config, &path, req.manual_sub)?;

    let headers_out = header_map(&location, &connection_id);
    let outcome = crate::control::api::JoinOutcome {
        state,
        answer: reply.answer,
        location,
    };

    debug_assert_eq!(outcome.state.participant_id, participant_id);
    debug_assert!(
        reply.epoch >= floor,
        "resume must not walk the connection epoch backwards"
    );

    let body = session_response(&s, &claims, &external_room_id, outcome, reply.epoch, now)?;

    // 201 means nothing was live under this id, so the participant was genuinely rebuilt.
    let status = if reply.existed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, headers_out, axum::Json(body)).into_response())
}

/// Leave a room, proving you hold the live connection.
pub(crate) async fn leave(
    s: AppState,
    external_room_id: ExternalRoomId,
    participant_id: ParticipantId,
    headers: HeaderMap,
) -> Result<Response, JsonApiError> {
    let now = now_secs();
    authorize(&s.api_config, &headers, &external_room_id, now)?;

    let connection_id: ConnectionId = headers
        .get(IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"'))
        .ok_or(ApiError::BadRequest("If-Match header required".into()))?
        .try_into()?;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    s.controller
        .try_send(
            controller::DeleteParticipant {
                room_id: crate::entity::RoomId::from_external(&external_room_id),
                participant_id,
                connection_id: Some(connection_id),
                reply: Some(reply_tx),
            }
            .into(),
        )
        .map_err(|e| match e {
            TrySendError::Full(_) => ApiError::RateLimited,
            TrySendError::Closed(_) => ApiError::ServiceUnavailable,
        })?;

    reply_rx
        .await
        .map_err(|_| ApiError::JoinError(ControllerError::ServiceUnavailable))?
        .map_err(ApiError::JoinError)?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(crate) fn require_json(headers: &HeaderMap) -> Result<(), ApiError> {
    if is_json(headers) {
        Ok(())
    } else {
        Err(ApiError::UnsupportedMediaType)
    }
}

/// Content-Type selects the representation. Matches on type/subtype only, so
/// `application/json; charset=utf-8` counts.
pub(crate) fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<mime::Mime>().ok())
        .is_some_and(|m| {
            m.type_() == mime::APPLICATION
                && (m.subtype() == mime::JSON || m.suffix() == Some(mime::JSON))
        })
}

/// DELETE carries no Content-Type, so JSON semantics are opted into by sending a credential or
/// explicitly asking for JSON. The legacy agent sends neither and keeps its unconditional 204.
pub(crate) fn wants_json_delete(headers: &HeaderMap) -> bool {
    headers.contains_key(IF_MATCH)
        || headers.contains_key(AUTHORIZATION)
        || headers
            .get(hyper::header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::api::{ApiConfig, router};
    use crate::control::auth::{
        AccessClaims, AuthConfig, JwtAlg, JwtKeyBytes, KeyId, PulseBeamClaims, ResumeKeyring,
        VerifyingKey,
    };
    use axum::body::Body;
    use http_body_util::BodyExt;
    use hyper::header::CONTENT_TYPE;
    use pulsebeam_runtime::mailbox;
    use pulsebeam_runtime::rand::seeded_rng;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    const ROOM: &str = "standup";
    const AUD: &str = "pulsebeam-test";
    const KID: &str = "key-2026-08";
    const ED25519_PUBLIC: [u8; 32] = [
        234, 74, 108, 99, 226, 156, 82, 10, 190, 245, 80, 123, 19, 46, 197, 249, 149, 71, 118, 174,
        190, 190, 123, 146, 66, 30, 234, 105, 20, 70, 210, 44,
    ];

    fn pkcs8() -> Vec<u8> {
        let mut der = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend_from_slice(&[7u8; 32]);
        der
    }

    fn auth_config() -> AuthConfig {
        AuthConfig::builder()
            .access_key(
                KeyId::new(KID).unwrap(),
                VerifyingKey::new(
                    JwtAlg::Ed25519,
                    JwtKeyBytes::Ed25519Raw(ED25519_PUBLIC.to_vec()),
                )
                .unwrap(),
            )
            .audience(AUD)
            .resume_keys(
                ResumeKeyring::new(vec![(KeyId::new("rk-1").unwrap(), [3u8; 32])]).unwrap(),
            )
            .build(&mut seeded_rng(1))
            .unwrap()
    }

    fn now() -> i64 {
        now_secs()
    }

    fn claims_for(room: &str, subject: &str) -> AccessClaims {
        AccessClaims {
            iss: "https://app.example.com".to_string(),
            sub: Identity::new(subject).unwrap(),
            aud: AUD.to_string(),
            exp: now() + 3600,
            nbf: None,
            iat: Some(now()),
            jti: "jti-1".to_string(),
            pb: PulseBeamClaims {
                room: room.to_string(),
                name: Some("Ada".to_string()),
                publish: true,
                subscribe: true,
                max_duration_secs: None,
            },
        }
    }

    fn sign(claims: &AccessClaims) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
        header.kid = Some(KID.to_string());
        jsonwebtoken::encode(
            &header,
            claims,
            &jsonwebtoken::EncodingKey::from_ed_der(&pkcs8()),
        )
        .unwrap()
    }

    fn token() -> String {
        sign(&claims_for(ROOM, "user_1042"))
    }

    fn offer_sdp() -> String {
        pulsebeam_testdata::RAW_CHROME_SDP.to_string()
    }

    fn answer() -> str0m::change::SdpAnswer {
        str0m::change::SdpAnswer::from_sdp_string(pulsebeam_testdata::RAW_CHROME_SDP).unwrap()
    }

    struct Harness {
        router: axum::Router,
        commands: Arc<Mutex<Vec<&'static str>>>,
    }

    /// `existed` controls whether the stubbed resume reports replacing a live participant.
    fn harness(auth: Option<AuthConfig>, existed: bool) -> Harness {
        let (tx, mut rx) = mailbox::new::<controller::ControllerCommand>(8);
        let commands = Arc::new(Mutex::new(Vec::new()));
        let seen = commands.clone();

        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    controller::ControllerCommand::CreateParticipant(_, reply) => {
                        seen.lock().unwrap().push("create");
                        let _ =
                            reply.send(Ok(controller::CreateParticipantReply { answer: answer() }));
                    }
                    controller::ControllerCommand::PatchParticipant(_, reply) => {
                        seen.lock().unwrap().push("patch");
                        let _ =
                            reply.send(Ok(controller::PatchParticipantReply { answer: answer() }));
                    }
                    controller::ControllerCommand::ResumeParticipant(m, reply) => {
                        seen.lock().unwrap().push("resume");
                        let _ = reply.send(Ok(controller::ResumeParticipantReply {
                            answer: answer(),
                            existed,
                            epoch: m.state.epoch,
                        }));
                    }
                    controller::ControllerCommand::DeleteParticipant(m) => {
                        seen.lock().unwrap().push("delete");
                        if let Some(reply) = m.reply {
                            let _ = reply.send(Ok(()));
                        }
                    }
                }
            }
        });

        let mut cfg = ApiConfig::new("/api/v1", "sfu.test");
        cfg.auth = auth.map(Arc::new);
        Harness {
            router: router(tx, cfg),
            commands,
        }
    }

    struct Captured {
        status: StatusCode,
        headers: HeaderMap,
        body: serde_json::Value,
        raw: String,
    }

    async fn send(h: &Harness, req: axum::http::Request<Body>) -> Captured {
        let response = h.router.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        let body = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
        Captured {
            status,
            headers,
            body,
            raw,
        }
    }

    fn json_post(
        room: &str,
        token: Option<&str>,
        body: serde_json::Value,
    ) -> axum::http::Request<Body> {
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/rooms/{room}/participants"))
            .header("host", "sfu.test")
            .header(CONTENT_TYPE, "application/json");
        if let Some(t) = token {
            req = req.header(AUTHORIZATION, format!("Bearer {t}"));
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    fn json_put(
        room: &str,
        participant: &ParticipantId,
        token: Option<&str>,
        body: serde_json::Value,
    ) -> axum::http::Request<Body> {
        let mut req = axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/api/v1/rooms/{room}/participants/{participant}"))
            .header("host", "sfu.test")
            .header(CONTENT_TYPE, "application/json");
        if let Some(t) = token {
            req = req.header(AUTHORIZATION, format!("Bearer {t}"));
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    fn join_body() -> serde_json::Value {
        serde_json::json!({ "sdp": offer_sdp() })
    }

    async fn join_ok(h: &Harness) -> Captured {
        let res = send(h, json_post(ROOM, Some(&token()), join_body())).await;
        assert_eq!(res.status, StatusCode::CREATED, "join failed: {}", res.raw);
        res
    }

    #[tokio::test]
    async fn a_valid_join_returns_the_session_and_mirrors_its_headers() {
        let h = harness(Some(auth_config()), false);
        let res = join_ok(&h).await;

        let body = &res.body;
        assert!(body["sdp"].as_str().unwrap().starts_with("v=0"));
        assert_eq!(body["room"], ROOM);
        assert!(body["participant_id"].as_str().unwrap().starts_with("pa_"));
        assert!(body["connection_id"].as_str().unwrap().starts_with("c_"));
        assert_eq!(body["epoch"], 0);
        assert!(!body["resume_token"].as_str().unwrap().is_empty());
        assert_eq!(body["identity"]["subject"], "user_1042");
        assert_eq!(body["capabilities"]["publish"], true);

        // The two representations must agree: resource mirrors Location, ETag mirrors connection_id.
        assert_eq!(
            res.headers.get("location").unwrap().to_str().unwrap(),
            body["resource"].as_str().unwrap()
        );
        assert_eq!(
            res.headers.get("etag").unwrap().to_str().unwrap(),
            body["connection_id"].as_str().unwrap()
        );
        assert_eq!(*h.commands.lock().unwrap(), vec!["create"]);
    }

    #[tokio::test]
    async fn every_rejected_request_stops_before_the_controller() {
        // The single most important property here: nothing unauthenticated reaches the actor.
        let expired = {
            // Well past the 60s leeway; a token 10s stale is deliberately still accepted.
            let mut c = claims_for(ROOM, "user_1042");
            c.exp = now() - 3600;
            c.iat = Some(now() - 7200);
            sign(&c)
        };
        let wrong_room = sign(&claims_for("retro", "user_1042"));
        let wrong_aud = {
            let mut c = claims_for(ROOM, "user_1042");
            c.aud = "someone-else".to_string();
            sign(&c)
        };
        let forged = {
            let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
            header.kid = Some(KID.to_string());
            jsonwebtoken::encode(
                &header,
                &claims_for(ROOM, "user_1042"),
                &jsonwebtoken::EncodingKey::from_secret(&ED25519_PUBLIC),
            )
            .unwrap()
        };

        let cases: Vec<(&str, Option<String>, StatusCode, &str)> = vec![
            ("no token", None, StatusCode::UNAUTHORIZED, "missing_token"),
            (
                "garbage",
                Some("nonsense".into()),
                StatusCode::UNAUTHORIZED,
                "malformed_token",
            ),
            (
                "expired",
                Some(expired),
                StatusCode::UNAUTHORIZED,
                "token_expired",
            ),
            (
                "wrong aud",
                Some(wrong_aud),
                StatusCode::UNAUTHORIZED,
                "invalid_audience",
            ),
            (
                "alg confusion",
                Some(forged),
                StatusCode::UNAUTHORIZED,
                "invalid_signature",
            ),
            (
                "wrong room",
                Some(wrong_room),
                StatusCode::FORBIDDEN,
                "room_mismatch",
            ),
        ];

        for (label, tok, status, code) in cases {
            let h = harness(Some(auth_config()), false);
            let res = send(&h, json_post(ROOM, tok.as_deref(), join_body())).await;
            assert_eq!(res.status, status, "{label}: {}", res.raw);
            assert_eq!(res.body["error"]["code"], code, "{label}");
            assert!(
                h.commands.lock().unwrap().is_empty(),
                "{label} reached the controller"
            );
            if status == StatusCode::UNAUTHORIZED {
                assert!(
                    res.headers.contains_key("www-authenticate"),
                    "{label} must carry a challenge"
                );
            }
        }
    }

    #[tokio::test]
    async fn the_json_surface_refuses_to_serve_when_auth_is_not_configured() {
        let h = harness(None, false);
        let res = send(&h, json_post(ROOM, Some(&token()), join_body())).await;
        assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.body["error"]["code"], "auth_not_configured");
        assert!(h.commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_malformed_body_is_rejected_with_a_stable_code() {
        let h = harness(Some(auth_config()), false);

        let not_json = send(
            &h,
            json_post(ROOM, Some(&token()), serde_json::json!("nope")),
        )
        .await;
        assert_eq!(not_json.status, StatusCode::BAD_REQUEST);
        assert_eq!(not_json.body["error"]["code"], "invalid_json");

        // deny_unknown_fields: a typo must fail loudly rather than be silently dropped.
        let typo = send(
            &h,
            json_post(
                ROOM,
                Some(&token()),
                serde_json::json!({"sdp": offer_sdp(), "manualsub": true}),
            ),
        )
        .await;
        assert_eq!(typo.status, StatusCode::BAD_REQUEST);
        assert_eq!(typo.body["error"]["code"], "invalid_json");

        let bad_sdp = send(
            &h,
            json_post(ROOM, Some(&token()), serde_json::json!({"sdp": "not sdp"})),
        )
        .await;
        assert_eq!(bad_sdp.status, StatusCode::BAD_REQUEST);
        assert_eq!(bad_sdp.body["error"]["code"], "invalid_sdp");

        assert!(h.commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resume_preserves_the_participant_and_advances_the_epoch() {
        let h = harness(Some(auth_config()), false);
        let joined = join_ok(&h).await;
        let participant: ParticipantId = joined.body["participant_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let resume_token = joined.body["resume_token"].as_str().unwrap().to_string();

        let res = send(
            &h,
            json_put(
                ROOM,
                &participant,
                Some(&token()),
                serde_json::json!({"sdp": offer_sdp(), "resume_token": resume_token}),
            ),
        )
        .await;

        // 201: nothing was live, so this is a genuine reconstruct.
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.raw);
        assert_eq!(
            res.body["participant_id"].as_str().unwrap(),
            participant.to_string(),
            "the participant id is what keeps every derived TrackId stable"
        );
        assert_eq!(res.body["epoch"], 1);
        assert_ne!(
            res.body["connection_id"].as_str().unwrap(),
            joined.body["connection_id"].as_str().unwrap()
        );
        assert_ne!(
            res.body["resume_token"].as_str().unwrap(),
            joined.body["resume_token"].as_str().unwrap(),
            "the resume token rotates so an active client always holds a fresh one"
        );
    }

    #[tokio::test]
    async fn resume_reports_200_when_it_replaced_a_live_participant() {
        let h = harness(Some(auth_config()), true);
        let joined = join_ok(&h).await;
        let participant: ParticipantId = joined.body["participant_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let resume_token = joined.body["resume_token"].as_str().unwrap().to_string();

        let res = send(
            &h,
            json_put(
                ROOM,
                &participant,
                Some(&token()),
                serde_json::json!({"sdp": offer_sdp(), "resume_token": resume_token}),
            ),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.raw);
    }

    #[tokio::test]
    async fn resume_is_bound_to_room_participant_and_subject() {
        let h = harness(Some(auth_config()), false);
        let joined = join_ok(&h).await;
        let participant: ParticipantId = joined.body["participant_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let resume_token = joined.body["resume_token"].as_str().unwrap().to_string();
        let body = serde_json::json!({"sdp": offer_sdp(), "resume_token": resume_token});

        // Another user's perfectly valid token must not adopt this participant.
        let intruder = sign(&claims_for(ROOM, "user_9999"));
        let res = send(
            &h,
            json_put(ROOM, &participant, Some(&intruder), body.clone()),
        )
        .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.raw);
        assert_eq!(res.body["error"]["code"], "subject_mismatch");

        // A different participant in the same room.
        let other = ParticipantId::new(&mut seeded_rng(77));
        let res = send(&h, json_put(ROOM, &other, Some(&token()), body.clone())).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN);
        assert_eq!(res.body["error"]["code"], "participant_mismatch");

        // An access token used where a resume token belongs.
        let swapped = serde_json::json!({"sdp": offer_sdp(), "resume_token": token()});
        let res = send(&h, json_put(ROOM, &participant, Some(&token()), swapped)).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED);
        assert_eq!(res.body["error"]["code"], "invalid_resume_token");

        assert_eq!(
            *h.commands.lock().unwrap(),
            vec!["create"],
            "no resume reached the controller"
        );
    }

    #[tokio::test]
    async fn capabilities_come_from_the_fresh_token_not_the_resume_token() {
        // The failure the slimmed ResumeClaims exists to prevent: a downgraded token must win.
        let h = harness(Some(auth_config()), false);
        let joined = join_ok(&h).await;
        let participant: ParticipantId = joined.body["participant_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let resume_token = joined.body["resume_token"].as_str().unwrap().to_string();

        let mut downgraded = claims_for(ROOM, "user_1042");
        downgraded.pb.publish = false;
        let res = send(
            &h,
            json_put(
                ROOM,
                &participant,
                Some(&sign(&downgraded)),
                serde_json::json!({"sdp": offer_sdp(), "resume_token": resume_token}),
            ),
        )
        .await;

        assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.raw);
        assert_eq!(res.body["error"]["code"], "publish_denied");
    }

    #[tokio::test]
    async fn put_requires_json() {
        let h = harness(Some(auth_config()), false);
        let participant = ParticipantId::new(&mut seeded_rng(5));
        let req = axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/api/v1/rooms/{ROOM}/participants/{participant}"))
            .header("host", "sfu.test")
            .header(CONTENT_TYPE, "application/sdp")
            .header(AUTHORIZATION, format!("Bearer {}", token()))
            .body(Body::from(offer_sdp()))
            .unwrap();
        let res = send(&h, req).await;
        assert_eq!(res.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(res.body["error"]["code"], "unsupported_media_type");
    }

    #[tokio::test]
    async fn a_charset_suffixed_content_type_still_selects_json() {
        let h = harness(Some(auth_config()), false);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/rooms/{ROOM}/participants"))
            .header("host", "sfu.test")
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .header(AUTHORIZATION, format!("Bearer {}", token()))
            .body(Body::from(join_body().to_string()))
            .unwrap();
        let res = send(&h, req).await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.raw);
    }

    #[tokio::test]
    async fn json_patch_is_not_a_renegotiation_surface() {
        let h = harness(Some(auth_config()), false);
        let participant = ParticipantId::new(&mut seeded_rng(6));
        let req = axum::http::Request::builder()
            .method("PATCH")
            .uri(format!("/api/v1/rooms/{ROOM}/participants/{participant}"))
            .header("host", "sfu.test")
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", token()))
            .body(Body::from(join_body().to_string()))
            .unwrap();
        let res = send(&h, req).await;
        assert_eq!(res.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(h.commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_with_a_credential_takes_the_json_path_and_verifies_it() {
        let h = harness(Some(auth_config()), false);
        let joined = join_ok(&h).await;
        let participant = joined.body["participant_id"].as_str().unwrap();
        let connection_id = joined.body["connection_id"].as_str().unwrap();

        let req = axum::http::Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/rooms/{ROOM}/participants/{participant}"))
            .header("host", "sfu.test")
            .header(AUTHORIZATION, format!("Bearer {}", token()))
            .header(IF_MATCH, connection_id)
            .body(Body::empty())
            .unwrap();
        let res = send(&h, req).await;

        assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.raw);
        assert_eq!(*h.commands.lock().unwrap(), vec!["create", "delete"]);
    }

    #[tokio::test]
    async fn require_auth_cannot_be_dodged_by_omitting_the_credential() {
        // Representation is chosen from headers the caller controls, so "no credential" must not
        // be a way to reach a path that does not ask for one.
        let mut cfg = ApiConfig::new("/api/v1", "sfu.test");
        cfg.auth = Some(Arc::new(auth_config()));
        cfg.require_auth = true;

        let (tx, mut rx) = mailbox::new::<controller::ControllerCommand>(8);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                if let controller::ControllerCommand::DeleteParticipant(m) = cmd {
                    recorder.lock().unwrap().push("delete");
                    if let Some(reply) = m.reply {
                        let _ = reply.send(Ok(()));
                    }
                }
            }
        });
        let h = Harness {
            router: router(tx, cfg),
            commands: seen,
        };

        let participant = ParticipantId::new(&mut seeded_rng(11));
        let sdp = offer_sdp();
        let requests = vec![
            axum::http::Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/rooms/{ROOM}/participants/{participant}"))
                .header("host", "sfu.test")
                .body(Body::empty())
                .unwrap(),
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/v1/rooms/{ROOM}/participants"))
                .header("host", "sfu.test")
                .header(CONTENT_TYPE, "application/sdp")
                .body(Body::from(sdp.clone()))
                .unwrap(),
            axum::http::Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/rooms/{ROOM}/participants/{participant}"))
                .header("host", "sfu.test")
                .header(CONTENT_TYPE, "application/sdp")
                .header(IF_MATCH, "c_R5T9K2ND7QW0J4XVA8ZP1MHC3B")
                .body(Body::from(sdp))
                .unwrap(),
        ];

        for req in requests {
            let method = req.method().clone();
            let res = send(&h, req).await;
            assert_eq!(
                res.status,
                StatusCode::UNAUTHORIZED,
                "unauthenticated {method} must be refused when require_auth is set"
            );
        }
        assert!(
            h.commands.lock().unwrap().is_empty(),
            "no unauthenticated request may reach the controller"
        );
    }

    #[tokio::test]
    async fn delete_without_a_credential_keeps_the_legacy_unconditional_behaviour() {
        // The existing agent sends neither If-Match nor Authorization and must be unaffected.
        let h = harness(Some(auth_config()), false);
        let participant = ParticipantId::new(&mut seeded_rng(9));
        let req = axum::http::Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/rooms/{ROOM}/participants/{participant}"))
            .header("host", "sfu.test")
            .body(Body::empty())
            .unwrap();
        let res = send(&h, req).await;
        assert_eq!(res.status, StatusCode::NO_CONTENT);
        assert_eq!(res.raw, "");
    }

    #[tokio::test]
    async fn every_json_error_parses_as_the_documented_envelope() {
        let h = harness(Some(auth_config()), false);
        for req in [
            json_post(ROOM, None, join_body()),
            json_post(ROOM, Some("garbage"), join_body()),
            json_post(ROOM, Some(&token()), serde_json::json!({"sdp": "x"})),
        ] {
            let res = send(&h, req).await;
            assert!(res.status.is_client_error() || res.status.is_server_error());
            let envelope: serde_json::Value = serde_json::from_str(&res.raw)
                .unwrap_or_else(|e| panic!("not JSON: {e}: {}", res.raw));
            let error = envelope
                .get("error")
                .unwrap_or_else(|| panic!("missing error envelope: {}", res.raw));
            assert!(error["code"].as_str().is_some_and(|c| !c.is_empty()));
            assert!(error["message"].as_str().is_some_and(|m| !m.is_empty()));
            assert!(
                error.get("retry_after_ms").is_none()
                    || res.status == StatusCode::TOO_MANY_REQUESTS
            );
        }
    }
}
