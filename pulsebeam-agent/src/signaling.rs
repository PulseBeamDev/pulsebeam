use http::Method;
use pulsebeam_core::net::{AsyncHttpClient, HttpError, HttpRequest};
use str0m::{
    change::{SdpAnswer, SdpOffer},
    error::SdpError,
};

#[derive(thiserror::Error, Debug)]
pub enum SignalingError {
    #[error("Http request failed: {0}")]
    Http(#[from] HttpError),
    #[error("Invalid uri: {0}")]
    InvalidUri(#[from] http::uri::InvalidUri),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("SDP error: {0}")]
    SdpError(#[from] SdpError),
}

pub struct HttpSignalingClient {
    http_client: Box<dyn AsyncHttpClient>,
    base_url: String,
    resource_url: Option<String>,
}

impl HttpSignalingClient {
    pub fn new(http_client: Box<dyn AsyncHttpClient>, base_url: impl Into<String>) -> Self {
        Self {
            http_client,
            base_url: base_url.into(),
            resource_url: None,
        }
    }

    pub async fn join(
        &mut self,
        room_id: &str,
        offer: SdpOffer,
    ) -> Result<SdpAnswer, SignalingError> {
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
            return Err(SignalingError::Protocol(format!(
                "Server rejected join: {}",
                res.status()
            )));
        }

        if let Some(loc) = res.headers().get("Location")
            && let Ok(loc_str) = loc.to_str()
        {
            self.resource_url = Some(loc_str.to_string());
        }

        let body_bytes = res.into_body();
        let answer_str = String::from_utf8(body_bytes)
            .map_err(|e| SignalingError::Protocol(format!("Invalid UTF-8 response: {}", e)))?;
        let answer = SdpAnswer::from_sdp_string(&answer_str)?;
        Ok(answer)
    }

    pub async fn leave(&mut self) -> Result<(), SignalingError> {
        if let Some(url) = self.resource_url.take() {
            tracing::info!(url, "Cleaning up remote session");
            let mut req = HttpRequest::new(vec![]);
            *req.uri_mut() = url.parse()?;
            *req.method_mut() = Method::DELETE;
            self.http_client.execute(req).await?;
        }

        Ok(())
    }
}
