use http::{Method, Response, Uri};
use pulsebeam_core::net::{AsyncHttpClient, HttpError, HttpRequest};
use str0m::{
    change::{SdpAnswer, SdpOffer},
    error::SdpError,
};

enum HeaderExt {
    ParticipantId,
}

impl HeaderExt {
    fn as_str(&self) -> &str {
        match self {
            Self::ParticipantId => "pb-participant-id",
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("Http request failed: {0}")]
    Http(#[from] HttpError),
    #[error("Invalid uri: {0}")]
    InvalidUri(#[from] http::uri::InvalidUri),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("SDP error: {0}")]
    SdpError(#[from] SdpError),
    /// A structured rejection from the JSON API, carrying the server's stable `code`.
    #[error("server rejected request ({status}): {code}: {message}")]
    Rejected {
        status: u16,
        code: String,
        message: String,
    },
    #[error("no access token available: {0}")]
    Token(String),
}

impl ApiError {
    /// The server's stable error code, when it sent one.
    pub fn code(&self) -> Option<&str> {
        match self {
            ApiError::Rejected { code, .. } => Some(code),
            _ => None,
        }
    }

    /// Retrying will not help: the credential is finished, or it never applied to this room.
    ///
    /// Without this the driver's exponential backoff would hammer a permanently doomed session.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.code(),
            Some(
                "resume_token_expired"
                    | "invalid_resume_token"
                    | "unknown_resume_kid"
                    | "room_mismatch"
                    | "subject_mismatch"
                    | "participant_mismatch"
                    | "token_expired"
                    | "invalid_signature"
                    | "unknown_kid"
                    | "publish_denied"
                    | "subscribe_denied"
            )
        )
    }
}

/// Supplies a currently-valid access token.
///
/// A resume can happen long after the original token expired, so the agent asks for a fresh one
/// each time rather than holding a string that goes stale mid-session.
pub trait AccessTokenSource: Send + Sync {
    fn token(&self) -> Result<String, ApiError>;
}

/// A fixed token, for short sessions and tests.
pub struct StaticToken(pub String);

impl AccessTokenSource for StaticToken {
    fn token(&self) -> Result<String, ApiError> {
        Ok(self.0.clone())
    }
}

impl<F> AccessTokenSource for F
where
    F: Fn() -> Result<String, ApiError> + Send + Sync,
{
    fn token(&self) -> Result<String, ApiError> {
        self()
    }
}

/// Which representation the client speaks. `Sdp` is the original WHIP-shaped surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    #[default]
    Sdp,
    Json,
}

#[derive(Debug, serde::Serialize)]
struct JoinRequest<'a> {
    sdp: &'a str,
    manual_sub: bool,
}

#[derive(Debug, serde::Serialize)]
struct ResumeRequest<'a> {
    sdp: &'a str,
    resume_token: &'a str,
    manual_sub: bool,
}

#[derive(Debug, serde::Deserialize)]
pub struct SessionResponse {
    pub sdp: String,
    pub room: String,
    pub participant_id: String,
    pub connection_id: String,
    pub epoch: u32,
    pub resource: String,
    pub resume_token: String,
    pub resume_expires_at: i64,
    pub session_expires_at: i64,
}

#[derive(Debug, serde::Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, serde::Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

/// Turns a non-2xx JSON response into a `Rejected` carrying the server's code, falling back to a
/// plain protocol error when the body is not an envelope.
fn json_error(resp: &Response<Vec<u8>>) -> ApiError {
    let status = resp.status().as_u16();
    match serde_json::from_slice::<ErrorEnvelope>(resp.body()) {
        Ok(envelope) => ApiError::Rejected {
            status,
            code: envelope.error.code,
            message: envelope.error.message,
        },
        Err(_) => ApiError::Protocol(format!(
            "server returned {status} with an unrecognised body"
        )),
    }
}

pub struct CreateParticipantRequest {
    pub offer: SdpOffer,
    pub room_id: String,
    pub manual_sub: bool,
}

pub struct CreateParticipantResponse {
    pub answer: SdpAnswer,
    pub resource_uri: Uri,
    pub participant_id: String,
    pub connection_id: Option<String>,
}

pub struct UpdateParticipantRequest {
    pub offer: SdpOffer,
    pub connection_id: Option<String>,
}

pub struct UpdateParticipantResponse {
    pub answer: SdpAnswer,
    pub connection_id: Option<String>,
}

fn connection_id_from_etag(resp: &Response<Vec<u8>>) -> Option<String> {
    resp.headers()
        .get(http::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
}

impl TryFrom<Response<Vec<u8>>> for UpdateParticipantResponse {
    type Error = ApiError;

    fn try_from(resp: Response<Vec<u8>>) -> Result<Self, Self::Error> {
        if !resp.status().is_success() {
            return Err(ApiError::Protocol(format!(
                "Server rejected update: {}",
                resp.status()
            )));
        }
        let connection_id = connection_id_from_etag(&resp);
        let body_str = std::str::from_utf8(resp.body())
            .map_err(|_| ApiError::Protocol("Body is not valid UTF-8".to_string()))?;

        let answer = SdpAnswer::from_sdp_string(body_str)?;

        Ok(UpdateParticipantResponse {
            answer,
            connection_id,
        })
    }
}

impl TryFrom<Response<Vec<u8>>> for CreateParticipantResponse {
    type Error = ApiError;

    fn try_from(resp: Response<Vec<u8>>) -> Result<Self, Self::Error> {
        if !resp.status().is_success() {
            return Err(ApiError::Protocol(format!(
                "Server rejected join: {}",
                resp.status()
            )));
        }
        let connection_id = connection_id_from_etag(&resp);
        let resource_uri = resp
            .headers()
            .get(http::header::LOCATION)
            .ok_or_else(|| ApiError::Protocol("Missing Location header".to_string()))?
            .to_str()
            .map_err(|_| ApiError::Protocol("Invalid UTF-8 in Location header".to_string()))?
            .parse::<Uri>()?;

        let body_str = std::str::from_utf8(resp.body())
            .map_err(|_| ApiError::Protocol("Body is not valid UTF-8".to_string()))?;

        let answer = SdpAnswer::from_sdp_string(body_str)?;

        let participant_id = resp
            .headers()
            .get(HeaderExt::ParticipantId.as_str())
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            // Fall back to parsing from the Location header if the header is missing.
            .or_else(|| {
                resource_uri
                    .path()
                    .rsplit('/')
                    .next()
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();

        Ok(CreateParticipantResponse {
            answer,
            resource_uri,
            participant_id,
            connection_id,
        })
    }
}

pub struct DeleteParticipantRequest {
    pub room_id: String,
    pub participant_id: String,
}

pub struct HttpApiClient {
    http_client: Box<dyn AsyncHttpClient>,
    base_uri: Uri,
    tokens: Option<Box<dyn AccessTokenSource>>,
    protocol: Protocol,
}

impl HttpApiClient {
    pub fn new(http_client: Box<dyn AsyncHttpClient>, base_uri: &str) -> Result<Self, ApiError> {
        let base_uri = format!("{}/api/v1", base_uri).parse()?;
        Ok(Self {
            http_client,
            base_uri,
            tokens: None,
            protocol: Protocol::Sdp,
        })
    }

    pub fn with_token_source(mut self, tokens: Box<dyn AccessTokenSource>) -> Self {
        self.tokens = Some(tokens);
        self
    }

    /// Opt into the JSON representation. Requires a token source.
    pub fn with_json_protocol(mut self) -> Self {
        self.protocol = Protocol::Json;
        self
    }

    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    fn authorization(&self) -> Result<String, ApiError> {
        let tokens = self
            .tokens
            .as_ref()
            .ok_or_else(|| ApiError::Token("no token source configured".to_string()))?;
        Ok(format!("Bearer {}", tokens.token()?))
    }

    fn json_request(
        &self,
        method: Method,
        uri: Uri,
        body: Vec<u8>,
    ) -> Result<HttpRequest, ApiError> {
        let mut req = HttpRequest::new(body);
        *req.uri_mut() = uri;
        *req.method_mut() = method;
        let headers = req.headers_mut();
        headers.insert("Content-Type", "application/json".parse().unwrap());
        headers.insert(http::header::ACCEPT, "application/json".parse().unwrap());
        headers.insert(
            http::header::AUTHORIZATION,
            self.authorization()?
                .parse()
                .map_err(|_| ApiError::Token("token is not a valid header value".to_string()))?,
        );
        Ok(req)
    }

    async fn send_json(&self, req: HttpRequest) -> Result<SessionResponse, ApiError> {
        let resp = self.http_client.execute(req).await?;
        if !resp.status().is_success() {
            return Err(json_error(&resp));
        }
        serde_json::from_slice(resp.body())
            .map_err(|e| ApiError::Protocol(format!("malformed session response: {e}")))
    }

    /// Join a room over the JSON API.
    pub async fn join_json(
        &self,
        room_id: &str,
        offer: SdpOffer,
        manual_sub: bool,
    ) -> Result<SessionResponse, ApiError> {
        let uri: Uri = format!("{}/rooms/{}/participants", self.base_uri, room_id).parse()?;
        let sdp = offer.to_sdp_string();
        let body = serde_json::to_vec(&JoinRequest {
            sdp: &sdp,
            manual_sub,
        })
        .map_err(|e| ApiError::Protocol(e.to_string()))?;
        tracing::info!(%uri, "joining over json");
        self.send_json(self.json_request(Method::POST, uri, body)?)
            .await
    }

    /// Re-establish a participant identity. Idempotent: it does not matter whether the
    /// participant is still live, or whether the node it lived on restarted.
    pub async fn resume_json(
        &self,
        resource_uri: Uri,
        offer: SdpOffer,
        resume_token: &str,
        manual_sub: bool,
    ) -> Result<SessionResponse, ApiError> {
        let sdp = offer.to_sdp_string();
        let body = serde_json::to_vec(&ResumeRequest {
            sdp: &sdp,
            resume_token,
            manual_sub,
        })
        .map_err(|e| ApiError::Protocol(e.to_string()))?;
        tracing::info!(uri = %resource_uri, "resuming over json");
        self.send_json(self.json_request(Method::PUT, resource_uri, body)?)
            .await
    }

    /// Leave, proving the caller holds the live connection.
    pub async fn leave_json(&self, resource_uri: Uri, connection_id: &str) -> Result<(), ApiError> {
        let mut req = HttpRequest::new(Vec::new());
        *req.uri_mut() = resource_uri;
        *req.method_mut() = Method::DELETE;
        let auth = self.authorization()?;
        let headers = req.headers_mut();
        headers.insert(http::header::ACCEPT, "application/json".parse().unwrap());
        headers.insert(
            http::header::AUTHORIZATION,
            auth.parse()
                .map_err(|_| ApiError::Token("token is not a valid header value".to_string()))?,
        );
        headers.insert(
            http::header::IF_MATCH,
            connection_id
                .parse()
                .map_err(|_| ApiError::Protocol("invalid connection id".to_string()))?,
        );

        let resp = self.http_client.execute(req).await?;
        if !resp.status().is_success() {
            return Err(json_error(&resp));
        }
        Ok(())
    }

    pub async fn create_participant(
        &self,
        req: CreateParticipantRequest,
    ) -> Result<CreateParticipantResponse, ApiError> {
        let uri = if req.manual_sub {
            format!(
                "{}/rooms/{}/participants?manual_sub=true",
                self.base_uri, req.room_id
            )
        } else {
            format!("{}/rooms/{}/participants", self.base_uri, req.room_id)
        };
        tracing::info!(%uri, "Sending SDP Offer");

        let raw_body = req.offer.to_sdp_string().into_bytes();
        let mut req = HttpRequest::new(raw_body);
        *req.uri_mut() = uri.parse()?;
        req.headers_mut()
            .insert("Content-Type", "application/sdp".parse().unwrap());
        *req.method_mut() = Method::POST;

        let res = self.http_client.execute(req).await?;
        res.try_into()
    }

    pub async fn update_participant(
        &self,
        uri: Uri,
        req: UpdateParticipantRequest,
    ) -> Result<UpdateParticipantResponse, ApiError> {
        tracing::info!(%uri, "Sending SDP Offer (Update)");

        let UpdateParticipantRequest {
            offer,
            connection_id,
        } = req;
        let raw_body = offer.to_sdp_string().into_bytes();
        let mut req = HttpRequest::new(raw_body);
        *req.uri_mut() = uri;
        req.headers_mut()
            .insert("Content-Type", "application/sdp".parse().unwrap());
        if let Some(connection_id) = connection_id {
            let value = connection_id
                .parse()
                .map_err(|_| ApiError::Protocol("Invalid connection id".to_string()))?;
            req.headers_mut().insert(http::header::IF_MATCH, value);
        }
        *req.method_mut() = Method::PATCH;

        let res = self.http_client.execute(req).await?;
        res.try_into()
    }

    pub async fn delete_participant(&self, req: DeleteParticipantRequest) -> Result<(), ApiError> {
        tracing::info!(
            room_id = req.room_id,
            participant_id = req.participant_id,
            "Cleaning up remote session"
        );
        let uri = format!(
            "{}/rooms/{}/participants/{}",
            self.base_uri, req.room_id, req.participant_id
        );
        self.delete_participant_by_uri(uri.parse()?).await
    }

    pub async fn delete_participant_by_uri(&self, uri: Uri) -> Result<(), ApiError> {
        let mut req = HttpRequest::new(vec![]);
        *req.uri_mut() = uri;
        *req.method_mut() = Method::DELETE;
        self.http_client.execute(req).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_core::net::{HttpResponse, HttpResult};
    use std::sync::Mutex;

    struct RecordingClient {
        requests: Mutex<Vec<HttpRequest>>,
        response: Mutex<Option<Response<Vec<u8>>>>,
    }

    impl RecordingClient {
        fn new(response: Response<Vec<u8>>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                response: Mutex::new(Some(response)),
            }
        }

        fn last_request(&self) -> HttpRequest {
            let mut requests = self.requests.lock().unwrap();
            requests.pop().expect("no request was issued")
        }
    }

    impl AsyncHttpClient for RecordingClient {
        fn execute(&self, req: HttpRequest) -> HttpResult<'_> {
            self.requests.lock().unwrap().push(req);
            let response = self.response.lock().unwrap().take();
            Box::pin(async move {
                response
                    .map(Ok)
                    .unwrap_or_else(|| -> HttpResponse { Err("no response queued".into()) })
            })
        }
    }

    fn sdp_body() -> Vec<u8> {
        pulsebeam_testdata::RAW_CHROME_SDP.as_bytes().to_vec()
    }

    fn offer() -> SdpOffer {
        SdpOffer::from_sdp_string(pulsebeam_testdata::RAW_CHROME_SDP).unwrap()
    }

    struct SharedRecorder(std::sync::Arc<RecordingClient>);

    impl AsyncHttpClient for SharedRecorder {
        fn execute(&self, req: HttpRequest) -> HttpResult<'_> {
            self.0.execute(req)
        }
    }

    fn client_with(
        response: Response<Vec<u8>>,
    ) -> (HttpApiClient, std::sync::Arc<RecordingClient>) {
        let recorder = std::sync::Arc::new(RecordingClient::new(response));
        let client = HttpApiClient::new(
            Box::new(SharedRecorder(recorder.clone())),
            "http://sfu.test",
        )
        .unwrap();
        (client, recorder)
    }

    #[tokio::test]
    async fn create_captures_connection_id_from_etag() {
        let response = Response::builder()
            .status(201)
            .header(
                http::header::LOCATION,
                "http://sfu.test/api/v1/rooms/r/participants/pa_1",
            )
            .header(http::header::ETAG, "c_ABC123")
            .body(sdp_body())
            .unwrap();
        let (client, _recorder) = client_with(response);

        let resp = client
            .create_participant(CreateParticipantRequest {
                offer: offer(),
                room_id: "r".to_string(),
                manual_sub: false,
            })
            .await
            .unwrap();

        assert_eq!(resp.connection_id.as_deref(), Some("c_ABC123"));
    }

    #[tokio::test]
    async fn create_tolerates_quoted_etag() {
        let response = Response::builder()
            .status(201)
            .header(
                http::header::LOCATION,
                "http://sfu.test/api/v1/rooms/r/participants/pa_1",
            )
            .header(http::header::ETAG, "\"c_ABC123\"")
            .body(sdp_body())
            .unwrap();
        let (client, _recorder) = client_with(response);

        let resp = client
            .create_participant(CreateParticipantRequest {
                offer: offer(),
                room_id: "r".to_string(),
                manual_sub: false,
            })
            .await
            .unwrap();

        assert_eq!(resp.connection_id.as_deref(), Some("c_ABC123"));
    }

    #[tokio::test]
    async fn update_sends_if_match_and_rotates_connection_id() {
        let response = Response::builder()
            .status(200)
            .header(http::header::ETAG, "c_ROTATED")
            .body(sdp_body())
            .unwrap();
        let (client, recorder) = client_with(response);

        let resp = client
            .update_participant(
                "http://sfu.test/api/v1/rooms/r/participants/pa_1"
                    .parse()
                    .unwrap(),
                UpdateParticipantRequest {
                    offer: offer(),
                    connection_id: Some("c_ORIGINAL".to_string()),
                },
            )
            .await
            .unwrap();

        let sent = recorder.last_request();
        assert_eq!(
            sent.headers().get(http::header::IF_MATCH).unwrap(),
            "c_ORIGINAL",
            "the server requires If-Match on PATCH; omitting it is a 400"
        );
        assert_eq!(resp.connection_id.as_deref(), Some("c_ROTATED"));
    }

    #[tokio::test]
    async fn update_omits_if_match_when_no_connection_id_is_known() {
        let response = Response::builder().status(200).body(sdp_body()).unwrap();
        let (client, recorder) = client_with(response);

        client
            .update_participant(
                "http://sfu.test/api/v1/rooms/r/participants/pa_1"
                    .parse()
                    .unwrap(),
                UpdateParticipantRequest {
                    offer: offer(),
                    connection_id: None,
                },
            )
            .await
            .unwrap();

        let sent = recorder.last_request();
        assert!(sent.headers().get(http::header::IF_MATCH).is_none());
    }

    fn json_client(
        response: Response<Vec<u8>>,
    ) -> (HttpApiClient, std::sync::Arc<RecordingClient>) {
        let recorder = std::sync::Arc::new(RecordingClient::new(response));
        let client = HttpApiClient::new(
            Box::new(SharedRecorder(recorder.clone())),
            "http://sfu.test",
        )
        .unwrap()
        .with_token_source(Box::new(StaticToken("test-token".to_string())))
        .with_json_protocol();
        (client, recorder)
    }

    fn session_json() -> Vec<u8> {
        serde_json::json!({
            "sdp": pulsebeam_testdata::RAW_CHROME_SDP,
            "room": "standup",
            "participant_id": "pa_8ZQ4W2P0H3RJ6VC1TKXE5N7BMD",
            "connection_id": "c_R5T9K2ND7QW0J4XVA8ZP1MHC3B",
            "epoch": 3,
            "resource": "http://sfu.test/api/v1/rooms/standup/participants/pa_8ZQ4W2P0H3RJ6VC1TKXE5N7BMD",
            "resume_token": "rt-next",
            "resume_expires_at": 1786294800i64,
            "session_expires_at": 1786298400i64,
            "identity": { "subject": "user_1042" },
            "capabilities": { "publish": true, "subscribe": true }
        })
        .to_string()
        .into_bytes()
    }

    #[tokio::test]
    async fn join_json_sends_a_bearer_token_and_parses_the_session() {
        let response = Response::builder()
            .status(201)
            .body(session_json())
            .unwrap();
        let (client, recorder) = json_client(response);

        let session = client.join_json("standup", offer(), false).await.unwrap();

        let sent = recorder.last_request();
        assert_eq!(sent.method(), Method::POST);
        assert_eq!(sent.headers()["content-type"], "application/json");
        assert_eq!(sent.headers()["authorization"], "Bearer test-token");
        assert_eq!(session.participant_id, "pa_8ZQ4W2P0H3RJ6VC1TKXE5N7BMD");
        assert_eq!(session.resume_token, "rt-next");
        assert_eq!(session.epoch, 3);
    }

    #[tokio::test]
    async fn resume_json_puts_to_the_resource_uri_with_the_token() {
        let response = Response::builder()
            .status(201)
            .body(session_json())
            .unwrap();
        let (client, recorder) = json_client(response);

        client
            .resume_json(
                "http://sfu.test/api/v1/rooms/standup/participants/pa_1"
                    .parse()
                    .unwrap(),
                offer(),
                "rt-current",
                false,
            )
            .await
            .unwrap();

        let sent = recorder.last_request();
        assert_eq!(
            sent.method(),
            Method::PUT,
            "resume is create-or-replace, not a patch"
        );
        let body: serde_json::Value = serde_json::from_slice(sent.body()).unwrap();
        assert_eq!(body["resume_token"], "rt-current");
        assert!(body["sdp"].as_str().unwrap().starts_with("v=0"));
        // The resume token travels in the body; the bearer still authorizes the request.
        assert_eq!(sent.headers()["authorization"], "Bearer test-token");
    }

    #[tokio::test]
    async fn a_rejection_carries_the_servers_stable_code() {
        let body = serde_json::json!({
            "error": { "code": "resume_token_expired", "message": "resume token has expired" }
        })
        .to_string()
        .into_bytes();
        let response = Response::builder().status(401).body(body).unwrap();
        let (client, _r) = json_client(response);

        let err = client
            .resume_json(
                "http://sfu.test/api/v1/rooms/standup/participants/pa_1"
                    .parse()
                    .unwrap(),
                offer(),
                "stale",
                false,
            )
            .await
            .unwrap_err();

        assert_eq!(err.code(), Some("resume_token_expired"));
        assert!(
            err.is_terminal(),
            "an expired resume token must stop the retry loop"
        );
    }

    #[test]
    fn terminal_and_retryable_failures_are_distinguished() {
        let rejected = |code: &str| ApiError::Rejected {
            status: 401,
            code: code.to_string(),
            message: String::new(),
        };

        // Retrying cannot mint a new credential, so these end the session.
        for code in [
            "resume_token_expired",
            "invalid_resume_token",
            "room_mismatch",
            "subject_mismatch",
            "participant_mismatch",
            "publish_denied",
        ] {
            assert!(rejected(code).is_terminal(), "{code} should be terminal");
        }

        // These are transient: the server is busy or briefly unavailable.
        for code in ["rate_limited", "service_unavailable", "internal"] {
            assert!(!rejected(code).is_terminal(), "{code} should be retryable");
        }
        assert!(!ApiError::Protocol("boom".into()).is_terminal());
    }

    #[tokio::test]
    async fn a_non_envelope_error_body_does_not_panic() {
        let response = Response::builder()
            .status(500)
            .body(b"<html>gateway</html>".to_vec())
            .unwrap();
        let (client, _r) = json_client(response);

        let err = client
            .join_json("standup", offer(), false)
            .await
            .unwrap_err();
        assert!(err.code().is_none());
        assert!(!err.is_terminal());
    }

    #[tokio::test]
    async fn json_calls_without_a_token_source_fail_before_any_request() {
        let response = Response::builder()
            .status(201)
            .body(session_json())
            .unwrap();
        let recorder = std::sync::Arc::new(RecordingClient::new(response));
        let client = HttpApiClient::new(
            Box::new(SharedRecorder(recorder.clone())),
            "http://sfu.test",
        )
        .unwrap()
        .with_json_protocol();

        let err = client
            .join_json("standup", offer(), false)
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Token(_)));
        assert!(
            recorder.requests.lock().unwrap().is_empty(),
            "an unauthenticated request must not be sent at all"
        );
    }
}
