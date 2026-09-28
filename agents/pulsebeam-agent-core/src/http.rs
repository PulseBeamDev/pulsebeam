use alloc::{string::String, vec::Vec};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub uri: String,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
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
                &if self.name.eq_ignore_ascii_case("authorization") {
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
    Delete,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}
