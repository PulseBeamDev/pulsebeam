use pulsebeam_agent_core::{HttpRequest, HttpResponse};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiRequest {
    pub request: HttpRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiResponse {
    pub response: HttpResponse,
}

pub trait HttpExecutor {
    fn execute(
        &mut self,
        request: HttpRequest,
    ) -> impl std::future::Future<Output = Result<HttpResponse, ApiError>> + Send;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiError {
    Transport(String),
    InvalidResponse(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(error) => write!(formatter, "HTTP transport: {error}"),
            Self::InvalidResponse(error) => write!(formatter, "HTTP response: {error}"),
        }
    }
}

impl std::error::Error for ApiError {}

#[derive(Default)]
pub struct InMemoryHttpExecutor {
    responses: std::collections::VecDeque<Result<HttpResponse, ApiError>>,
    requests: Vec<HttpRequest>,
}

impl InMemoryHttpExecutor {
    pub fn push_response(&mut self, response: Result<HttpResponse, ApiError>) {
        self.responses.push_back(response);
    }

    pub fn requests(&self) -> &[HttpRequest] {
        &self.requests
    }
}

impl HttpExecutor for InMemoryHttpExecutor {
    async fn execute(&mut self, request: HttpRequest) -> Result<HttpResponse, ApiError> {
        self.requests.push(request);
        self.responses
            .pop_front()
            .unwrap_or_else(|| Err(ApiError::Transport("no response queued".to_owned())))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pulsebeam_agent_core::HttpMethod;

    #[tokio::test]
    async fn executor_preserves_owned_request_order() {
        let mut executor = InMemoryHttpExecutor::default();
        executor.push_response(Ok(HttpResponse::new(204, Vec::new())));
        let request = HttpRequest::new(HttpMethod::Delete, "/participant", Vec::new());
        assert_eq!(
            executor.execute(request.clone()).await,
            Ok(HttpResponse::new(204, Vec::new()))
        );
        assert_eq!(executor.requests(), &[request]);
    }
}
