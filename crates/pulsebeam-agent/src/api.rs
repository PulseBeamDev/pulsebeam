use http::{HeaderValue, Method, Response, Uri};
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
    /// The connection's generation, which the next `PATCH` must echo as `If-Match`.
    ///
    /// This is what makes a reconnect a reconnect: the participant id stays, and the server tells
    /// the two connections apart by generation. Without it the server rejects the update and the
    /// only way back into the room is as a brand new participant - a second tile for one person.
    pub etag: String,
}

pub struct UpdateParticipantRequest {
    pub offer: SdpOffer,
    /// The generation being replaced, sent as `If-Match`. Required by the server.
    pub etag: String,
}

pub struct UpdateParticipantResponse {
    pub answer: SdpAnswer,
    /// The new generation, to be echoed by the reconnect after this one.
    pub etag: String,
}

/// The connection generation the server just handed out.
fn read_etag(resp: &Response<Vec<u8>>) -> Result<String, ApiError> {
    let etag = resp
        .headers()
        .get(http::header::ETAG)
        .ok_or_else(|| ApiError::Protocol("Missing ETag header".to_string()))?
        .to_str()
        .map_err(|_| ApiError::Protocol("Invalid UTF-8 in ETag header".to_string()))?;
    Ok(etag.trim_matches('"').to_string())
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
        let body_str = std::str::from_utf8(resp.body())
            .map_err(|_| ApiError::Protocol("Body is not valid UTF-8".to_string()))?;

        let answer = SdpAnswer::from_sdp_string(body_str)?;
        let etag = read_etag(&resp)?;

        Ok(UpdateParticipantResponse { answer, etag })
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
            .map(std::string::ToString::to_string)
            // Fall back to parsing from the Location header if the header is missing.
            .or_else(|| {
                resource_uri
                    .path()
                    .rsplit('/')
                    .next()
                    .map(std::string::ToString::to_string)
            })
            .unwrap_or_default();

        let etag = read_etag(&resp)?;

        Ok(CreateParticipantResponse {
            answer,
            resource_uri,
            participant_id,
            etag,
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
}

impl HttpApiClient {
    pub fn new(http_client: Box<dyn AsyncHttpClient>, base_uri: &str) -> Result<Self, ApiError> {
        let base_uri = format!("{base_uri}/api/v1").parse()?;
        Ok(Self {
            http_client,
            base_uri,
        })
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
            .insert("Content-Type", HeaderValue::from_static("application/sdp"));
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

        let req_etag = req.etag.clone();
        let raw_body = req.offer.to_sdp_string().into_bytes();
        let mut req = HttpRequest::new(raw_body);
        *req.uri_mut() = uri;
        req.headers_mut()
            .insert("Content-Type", HeaderValue::from_static("application/sdp"));
        // The server needs to know which connection this replaces, and refuses the update without
        // it. Omitting it made every reconnect fail with 400 and the client retry forever.
        req.headers_mut().insert(
            http::header::IF_MATCH,
            HeaderValue::from_str(&req_etag)
                .map_err(|_| ApiError::Protocol("ETag is not a valid header value".into()))?,
        );
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
