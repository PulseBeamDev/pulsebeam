use std::collections::VecDeque;

use pulsebeam_agent_core::{HttpRequest, HttpResponse};

use crate::interop::WebError;

pub struct FetchClient {
    #[cfg(not(target_arch = "wasm32"))]
    mock: MockFetch,
}

impl FetchClient {
    #[cfg(target_arch = "wasm32")]
    pub fn browser() -> Self {
        Self {}
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn mock() -> Self {
        Self {
            mock: MockFetch::default(),
        }
    }

    pub async fn execute(&mut self, request: HttpRequest) -> Result<HttpResponse, WebError> {
        #[cfg(target_arch = "wasm32")]
        {
            execute_browser(request).await
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.mock.execute(request)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn mock_mut(&mut self) -> &mut MockFetch {
        &mut self.mock
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for FetchClient {
    fn default() -> Self {
        Self::mock()
    }
}

#[cfg(target_arch = "wasm32")]
async fn execute_browser(request: HttpRequest) -> Result<HttpResponse, WebError> {
    use js_sys::Uint8Array;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Headers, Request, RequestInit, Response};

    let headers = Headers::new().map_err(js_error)?;
    for header in request.headers {
        headers.set(&header.name, &header.value).map_err(js_error)?;
    }
    let init = RequestInit::new();
    init.set_method(&request.method.to_string());
    init.set_headers(&headers);
    if !request.body.is_empty() {
        let body = Uint8Array::from(request.body.as_slice());
        init.set_body(Some(body.as_ref()));
    }
    let browser_request = Request::new_with_str_and_init(&request.uri, &init).map_err(js_error)?;
    let window =
        web_sys::window().ok_or_else(|| WebError::Http("window unavailable".to_owned()))?;
    let response = JsFuture::from(window.fetch_with_request(&browser_request))
        .await
        .map_err(js_error)?
        .dyn_into::<Response>()
        .map_err(|_| WebError::Http("fetch returned a non-Response value".to_owned()))?;
    let body = JsFuture::from(response.array_buffer().map_err(js_error)?)
        .await
        .map_err(js_error)?;
    let bytes = Uint8Array::new(&body).to_vec();
    let mut output = HttpResponse::new(response.status(), bytes);
    for name in ["Location", "ETag", "Content-Type"] {
        if let Some(value) = response.headers().get(name).map_err(js_error)? {
            output
                .headers
                .push(pulsebeam_agent_core::HttpHeader::new(name, value));
        }
    }
    Ok(output)
}

#[cfg(target_arch = "wasm32")]
fn js_error(error: wasm_bindgen::JsValue) -> WebError {
    WebError::Http(
        error
            .as_string()
            .unwrap_or_else(|| "browser fetch failed".to_owned()),
    )
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub struct MockFetch {
    responses: VecDeque<Result<HttpResponse, WebError>>,
    requests: Vec<HttpRequest>,
}

#[cfg(not(target_arch = "wasm32"))]
impl MockFetch {
    pub fn push_response(&mut self, response: Result<HttpResponse, WebError>) {
        self.responses.push_back(response);
    }

    pub fn requests(&self) -> &[HttpRequest] {
        &self.requests
    }

    fn execute(&mut self, request: HttpRequest) -> Result<HttpResponse, WebError> {
        self.requests.push(request);
        self.responses
            .pop_front()
            .unwrap_or_else(|| Err(WebError::Http("no mocked response".to_owned())))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pulsebeam_agent_core::HttpMethod;

    #[test]
    fn mock_fetch_preserves_owned_requests() {
        let mut fetch = FetchClient::mock();
        fetch
            .mock_mut()
            .push_response(Ok(HttpResponse::new(204, Vec::new())));
        let request = HttpRequest::new(HttpMethod::Delete, "/participant", Vec::new());
        assert_eq!(
            fetch.mock_mut().execute(request.clone()),
            Ok(HttpResponse::new(204, Vec::new()))
        );
        assert_eq!(fetch.mock_mut().requests(), &[request]);
    }
}
