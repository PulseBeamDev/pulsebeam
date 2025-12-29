use reqwest::Client as HttpClient;
use str0m::{
    change::{SdpAnswer, SdpOffer},
    error::SdpError,
};

#[derive(thiserror::Error, Debug)]
pub enum SignalingError {
    #[error("Http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("SDP error: {0}")]
    SdpError(#[from] SdpError),
}

pub struct HttpSignalingClient {
    http_client: HttpClient,
    base_url: String,
    resource_url: Option<String>,
}

impl Default for HttpSignalingClient {
    fn default() -> Self {
        Self::new(HttpClient::new(), "http://locahost:3000")
    }
}

impl HttpSignalingClient {
    pub fn new(http_client: HttpClient, base_url: impl Into<String>) -> Self {
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

        let res = self
            .http_client
            .post(&uri)
            .header("Content-Type", "application/sdp")
            .body(offer.to_sdp_string())
            .send()
            .await?;

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

        let answer = res.text().await?;
        let answer = SdpAnswer::from_sdp_string(&answer)?;
        Ok(answer)
    }

    pub async fn leave(&mut self) {
        if let Some(url) = self.resource_url.take() {
            tracing::info!(url, "Cleaning up remote session");
            let _ = self.http_client.delete(url).send().await;
        }
    }
}
