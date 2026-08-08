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
}

impl HttpApiClient {
    pub fn new(http_client: Box<dyn AsyncHttpClient>, base_uri: &str) -> Result<Self, ApiError> {
        let base_uri = format!("{}/api/v1", base_uri).parse()?;
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
}
