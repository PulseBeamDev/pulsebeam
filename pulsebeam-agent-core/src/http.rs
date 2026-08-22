use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpMethod {
    Get,
    Post,
    Patch,
    Delete,
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let method = match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        };
        formatter.write_str(method)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

impl HttpHeader {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub uri: String,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn new(method: HttpMethod, uri: impl Into<String>, body: Vec<u8>) -> Self {
        let uri = uri.into();
        debug_assert!(!uri.is_empty());
        Self {
            method,
            uri,
            headers: Vec::new(),
            body,
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.set_header(name, value);
        self
    }

    pub fn set_header(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        if let Some(header) = self
            .headers
            .iter_mut()
            .find(|header| header.name.eq_ignore_ascii_case(&name))
        {
            header.value = value.into();
            return;
        }
        self.headers.push(HttpHeader::new(name, value));
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(HttpHeader::new(name, value));
        self
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }

    pub fn require_success(self) -> Result<Self, HttpStatusError> {
        if self.is_success() {
            return Ok(self);
        }
        Err(HttpStatusError {
            status: self.status,
            body: self.body,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpStatusError {
    pub status: u16,
    pub body: Vec<u8>,
}

impl fmt::Display for HttpStatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "HTTP status {}", self.status)
    }
}

impl std::error::Error for HttpStatusError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_are_owned_and_case_insensitive() {
        let request = HttpRequest::new(HttpMethod::Patch, "/participant", vec![1])
            .with_header("If-Match", "etag-1");
        assert_eq!(request.header("if-match"), Some("etag-1"));
    }

    #[test]
    fn non_success_response_keeps_body_for_the_caller() {
        let response = HttpResponse::new(409, vec![4, 2]);
        assert_eq!(
            response.require_success(),
            Err(HttpStatusError {
                status: 409,
                body: vec![4, 2],
            })
        );
    }
}
