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
            Self::ParticipantId => "PB-Participant-Id",
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
}

pub struct CreateParticipantResponse {
    pub answer: SdpAnswer,
    pub resource_uri: Uri,
    pub participant_id: String,
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

        let participant_id = resp
            .headers()
            .get(HeaderExt::ParticipantId.as_str())
            .ok_or_else(|| ApiError::Protocol("Missing PB-Participant-Id header".to_string()))?
            .to_str()
            .map_err(|_| ApiError::Protocol("Invalid UTF-8 in Participant-Id header".to_string()))?
            .to_string();

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

        Ok(CreateParticipantResponse {
            answer,
            resource_uri,
            participant_id,
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
    pub fn new(http_client: Box<dyn AsyncHttpClient>, base_url: &str) -> Result<Self, ApiError> {
        Ok(Self {
            http_client,
            base_uri: base_url.parse()?,
        })
    }

    pub async fn create_participant(
        &self,
        req: CreateParticipantRequest,
    ) -> Result<CreateParticipantResponse, ApiError> {
        let uri = format!("{}api/v1/rooms/{}/participants", self.base_uri, req.room_id);
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

    pub async fn delete_participant(&self, req: DeleteParticipantRequest) -> Result<(), ApiError> {
        tracing::info!(
            room_id = req.room_id,
            participant_id = req.participant_id,
            "Cleaning up remote session"
        );
        let uri = format!(
            "{}api/v1/rooms/{}/participants/{}",
            self.base_uri, req.room_id, req.participant_id
        );
        let mut req = HttpRequest::new(vec![]);
        *req.uri_mut() = uri.parse()?;
        *req.method_mut() = Method::DELETE;
        self.http_client.execute(req).await?;

        Ok(())
    }
}
