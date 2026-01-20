use http::{Method, Uri};
use pulsebeam_core::net::{AsyncHttpClient, HttpError, HttpRequest};
use str0m::{
    change::{SdpAnswer, SdpOffer},
    error::SdpError,
};

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

pub struct HttpApiClient {
    http_client: Box<dyn AsyncHttpClient>,
    base_url: String,
}

impl HttpApiClient {
    pub fn new(http_client: Box<dyn AsyncHttpClient>, base_url: impl Into<String>) -> Self {
        Self {
            http_client,
            base_url: base_url.into(),
        }
    }

    pub async fn connect(
        &self,
        room_id: &str,
        offer: SdpOffer,
    ) -> Result<(SdpAnswer, Option<Uri>), ApiError> {
        let uri = format!("{}/api/v1/rooms/{}", self.base_url, room_id);
        tracing::info!(%uri, "Sending SDP Offer");

        let raw_body = offer.to_sdp_string().into_bytes();
        let mut req = HttpRequest::new(raw_body);
        *req.uri_mut() = uri.parse()?;
        req.headers_mut()
            .insert("Content-Type", "application/sdp".parse().unwrap());
        *req.method_mut() = Method::POST;

        let res = self.http_client.execute(req).await?;
        if !res.status().is_success() {
            return Err(ApiError::Protocol(format!(
                "Server rejected join: {}",
                res.status()
            )));
        }

        let resource_uri = if let Some(loc) = res.headers().get("Location")
            && let Ok(loc_str) = loc.to_str()
        {
            Some(loc_str.parse()?)
        } else {
            None
        };

        let body_bytes = res.into_body();
        let answer_str = String::from_utf8(body_bytes)
            .map_err(|e| ApiError::Protocol(format!("Invalid UTF-8 response: {}", e)))?;
        let answer = SdpAnswer::from_sdp_string(&answer_str)?;
        Ok((answer, resource_uri))
    }

    pub async fn disconnect(&self, resource_uri: Uri) -> Result<(), ApiError> {
        tracing::info!(%resource_uri, "Cleaning up remote session");
        let mut req = HttpRequest::new(vec![]);
        *req.uri_mut() = resource_uri;
        *req.method_mut() = Method::DELETE;
        self.http_client.execute(req).await?;

        Ok(())
    }
}
