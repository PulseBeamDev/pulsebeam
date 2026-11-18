use alloc::{string::String, vec::Vec};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub uri: String,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

impl core::fmt::Debug for HttpRequest {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("uri", &self.uri)
            .field("headers", &self.headers)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

impl core::fmt::Debug for HttpHeader {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("HttpHeader")
            .field("name", &self.name)
            .field(
                "value",
                &if self.name.eq_ignore_ascii_case("authorization")
                    || self.name.eq_ignore_ascii_case("pulsebeam-owner")
                {
                    "[REDACTED]"
                } else {
                    &self.value
                },
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum HttpMethod {
    Post,
    Patch,
    Delete,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

impl core::fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}
