use alloc::{string::String, vec::Vec};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub uri: String,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum HttpMethod {
    Post,
    Patch,
    Delete,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}
