use alloc::{string::String, vec::Vec};

pub struct HttpRequest {
    pub method: HttpMethod,
    pub uri: String,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}
