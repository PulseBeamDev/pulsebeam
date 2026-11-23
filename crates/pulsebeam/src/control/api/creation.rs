mod delivery;
pub(crate) use delivery::{Delivery, DeliveryBudget, MAX_DELIVERIES};

use std::{
    io::{self, Write},
    num::NonZeroU64,
};

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use hyper::header::{CONTENT_ENCODING, CONTENT_TYPE, LOCATION};
use pulsebeam_core::auth::VerifiedAuthorization;
use pulsebeam_proto::state::RtpCapabilities;

use super::NativeResponse;
use crate::{
    control::controller::ownership::{MAX_RESULT_BYTES, Proof},
    entity::ConnectionId,
};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PreflightError {
    #[error("exactly one canonical owner proof and attempt are required")]
    Ownership,
    #[error("malformed Accept-Encoding header")]
    AcceptEncoding,
    #[error("cached creation responses require acceptable identity encoding")]
    IdentityRequired,
}

pub(crate) fn ownership_headers(
    headers: &HeaderMap,
) -> Result<(Proof, NonZeroU64), PreflightError> {
    let one = |name| {
        let mut values = headers.get_all(name).iter();
        let value = values.next().ok_or(PreflightError::Ownership)?;
        if values.next().is_some() {
            return Err(PreflightError::Ownership);
        }
        value.to_str().map_err(|_| PreflightError::Ownership)
    };
    let proof = Proof::parse(one("pulsebeam-owner")?).map_err(|_| PreflightError::Ownership)?;
    let attempt = one("pulsebeam-attempt")?;
    if !matches!(attempt.as_bytes().first(), Some(b'1'..=b'9'))
        || attempt.len() > 20
        || !attempt.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(PreflightError::Ownership);
    }
    let attempt = attempt.parse().map_err(|_| PreflightError::Ownership)?;
    Ok((proof, attempt))
}

pub(crate) fn require_identity(headers: &HeaderMap) -> Result<(), PreflightError> {
    let mut explicit_identity = false;
    let mut wildcard_denial = false;
    for value in headers.get_all(hyper::header::ACCEPT_ENCODING) {
        let value = value.to_str().map_err(|_| PreflightError::AcceptEncoding)?;
        for item in value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            let (coding, quality) = item.split_once(';').unwrap_or((item, "q=1"));
            let coding = coding.trim();
            if coding.is_empty()
                || !coding
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            {
                return Err(PreflightError::AcceptEncoding);
            }
            let quality = quality.trim();
            let quality = quality
                .strip_prefix("q=")
                .or_else(|| quality.strip_prefix("Q="))
                .ok_or(PreflightError::AcceptEncoding)?;
            let (whole, fraction) = quality.split_once('.').unwrap_or((quality, ""));
            if !matches!(whole, "0" | "1")
                || fraction.len() > 3
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
                || (whole == "1" && fraction.bytes().any(|byte| byte != b'0'))
            {
                return Err(PreflightError::AcceptEncoding);
            }
            let positive = whole == "1" || fraction.bytes().any(|byte| byte != b'0');
            if coding.eq_ignore_ascii_case("identity") {
                // Ambiguous duplicate denials must not turn an admitted cached 201 into a 406.
                if !positive {
                    return Err(PreflightError::IdentityRequired);
                }
                explicit_identity = true;
            } else if coding == "*" && !positive {
                wildcard_denial = true;
            }
        }
    }
    if wildcard_denial && !explicit_identity {
        return Err(PreflightError::IdentityRequired);
    }
    Ok(())
}

const NATIVE_PREFIX: &[u8] = b"HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-encoding: identity\r\nlocation: ";
const SDP_PREFIX: &[u8] = b"HTTP/1.1 201 Created\r\ncontent-type: application/sdp\r\ncontent-encoding: identity\r\nlocation: ";
const SEPARATOR: &[u8] = b"\r\n\r\n";

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    #[error("complete creation response exceeds its byte limit")]
    TooLarge,
    #[error("creation response cannot be serialized")]
    Serialization,
    #[error("invalid cached creation response")]
    InvalidImage,
}

enum CreationBody<'a> {
    Native(NativeResponse<'a>),
    Sdp(&'a str),
}

pub(crate) struct CreationResponse<'a> {
    location: &'a HeaderValue,
    body: CreationBody<'a>,
}

pub(crate) struct PreparedCreation<'a> {
    response: CreationResponse<'a>,
    size: usize,
}

impl<'a> CreationResponse<'a> {
    pub(crate) fn native(
        location: &'a HeaderValue,
        authorization: &'a VerifiedAuthorization,
        connection: ConnectionId,
        capabilities: &'a RtpCapabilities,
        answer: &'a str,
        data_profile: crate::control::native::DataProfile,
    ) -> Self {
        Self {
            location,
            body: CreationBody::Native(NativeResponse::new(
                authorization,
                connection,
                capabilities,
                answer,
                data_profile,
            )),
        }
    }

    pub(crate) fn sdp(location: &'a HeaderValue, answer: &'a str) -> Self {
        Self {
            location,
            body: CreationBody::Sdp(answer),
        }
    }

    pub(crate) fn validate(self) -> Result<PreparedCreation<'a>, Error> {
        if self.location.is_empty()
            || matches!(&self.body, CreationBody::Sdp(answer) if answer.is_empty())
        {
            return Err(Error::Serialization);
        }
        let mut writer = LimitedWriter::new(io::sink(), MAX_RESULT_BYTES);
        self.write_to(&mut writer).map_err(|_| {
            if writer.exceeded {
                Error::TooLarge
            } else {
                Error::Serialization
            }
        })?;
        Ok(PreparedCreation {
            response: self,
            size: writer.size,
        })
    }

    fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(match &self.body {
            CreationBody::Native(_) => NATIVE_PREFIX,
            CreationBody::Sdp(_) => SDP_PREFIX,
        })?;
        writer.write_all(self.location.as_bytes())?;
        writer.write_all(SEPARATOR)?;
        match &self.body {
            CreationBody::Native(response) => {
                serde_json::to_writer(writer, response).map_err(io::Error::other)
            }
            CreationBody::Sdp(answer) => writer.write_all(answer.as_bytes()),
        }
    }
}

impl PreparedCreation<'_> {
    // Validate before claiming ownership, then encode these same immutable facts under its reservation.
    pub(crate) fn encode(self) -> Vec<u8> {
        let mut writer = LimitedWriter::new(Vec::with_capacity(self.size), self.size);
        self.response.write_to(&mut writer).unwrap_or_else(|_| {
            pulsebeam_runtime::fatal!(
                "validated immutable creation response changed during encoding"
            );
        });
        if writer.size != self.size {
            pulsebeam_runtime::fatal!("validated creation response changed its encoded length");
        }
        writer.target
    }
}

struct LimitedWriter<W> {
    target: W,
    size: usize,
    limit: usize,
    exceeded: bool,
}

impl<W> LimitedWriter<W> {
    fn new(target: W, limit: usize) -> Self {
        Self {
            target,
            size: 0,
            limit,
            exceeded: false,
        }
    }
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .size
            .checked_add(bytes.len())
            .is_none_or(|size| size > self.limit)
        {
            self.exceeded = true;
            return Err(io::Error::other("creation response byte limit exceeded"));
        }
        let written = self.target.write(bytes)?;
        self.size = self
            .size
            .checked_add(written)
            .filter(|size| *size <= self.limit)
            .ok_or_else(|| io::Error::other("creation response writer exceeded its byte limit"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.target.flush()
    }
}

pub(crate) fn into_response(result: Delivery) -> Result<Response, Error> {
    if result.as_ref().len() > MAX_RESULT_BYTES {
        return Err(Error::InvalidImage);
    }
    let encoded = result.into_bytes();
    let (prefix, content_type) = if encoded.starts_with(NATIVE_PREFIX) {
        (NATIVE_PREFIX, "application/json")
    } else if encoded.starts_with(SDP_PREFIX) {
        (SDP_PREFIX, "application/sdp")
    } else {
        return Err(Error::InvalidImage);
    };
    let location_start = prefix.len();
    let location_end = encoded
        .get(location_start..)
        .ok_or(Error::InvalidImage)?
        .windows(SEPARATOR.len())
        .position(|bytes| bytes == SEPARATOR)
        .and_then(|offset| location_start.checked_add(offset))
        .ok_or(Error::InvalidImage)?;
    let body_start = location_end
        .checked_add(SEPARATOR.len())
        .ok_or(Error::InvalidImage)?;
    if body_start == encoded.len() {
        return Err(Error::InvalidImage);
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        LOCATION,
        HeaderValue::from_maybe_shared(encoded.slice(location_start..location_end))
            .map_err(|_| Error::InvalidImage)?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    // The cached byte owner must survive delivery, rather than a compressor copying it into uncharged storage.
    headers.insert(CONTENT_ENCODING, HeaderValue::from_static("identity"));
    Ok((
        StatusCode::CREATED,
        headers,
        Body::from(encoded.slice(body_start..)),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    async fn assert_negotiated(probe: &axum::Router, headers: HeaderMap) {
        let mut request = axum::http::Request::new(Body::empty());
        *request.headers_mut() = headers;
        let response = probe.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers().get(CONTENT_ENCODING).unwrap(),
            "identity"
        );
    }

    #[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
    async fn ownership_preflight_requires_canonical_headers_and_identity() {
        let probe = axum::Router::new()
            .route(
                "/",
                axum::routing::get(|| async {
                    (
                        StatusCode::CREATED,
                        [(CONTENT_ENCODING, "identity")],
                        "cached creation response",
                    )
                }),
            )
            .layer(tower_http::compression::CompressionLayer::new());
        let mut headers = HeaderMap::new();
        let proof = "0".repeat(52);
        assert_eq!(ownership_headers(&headers), Err(PreflightError::Ownership));
        headers.insert("pulsebeam-owner", HeaderValue::from_str(&proof).unwrap());
        assert_eq!(ownership_headers(&headers), Err(PreflightError::Ownership));
        for value in ["1", "18446744073709551615"] {
            headers.insert("pulsebeam-attempt", HeaderValue::from_str(value).unwrap());
            let (parsed, attempt) = ownership_headers(&headers).unwrap();
            assert_eq!(parsed, Proof::parse(&proof).unwrap());
            assert_eq!(attempt.to_string(), value);
        }
        for value in [
            "",
            "0",
            "01",
            "+1",
            " 1",
            "1 ",
            "-1",
            "1e0",
            "18446744073709551616",
        ] {
            headers.insert("pulsebeam-attempt", HeaderValue::from_str(value).unwrap());
            assert_eq!(ownership_headers(&headers), Err(PreflightError::Ownership));
        }
        headers.insert("pulsebeam-attempt", HeaderValue::from_static("1"));
        for name in ["pulsebeam-owner", "pulsebeam-attempt"] {
            let mut duplicated = headers.clone();
            duplicated.append(name, headers.get(name).unwrap().clone());
            assert_eq!(
                ownership_headers(&duplicated),
                Err(PreflightError::Ownership)
            );
        }
        headers.insert("pulsebeam-owner", HeaderValue::from_static("bad"));
        assert_eq!(ownership_headers(&headers), Err(PreflightError::Ownership));
        assert_eq!(require_identity(&HeaderMap::new()), Ok(()));
        assert_negotiated(&probe, HeaderMap::new()).await;
        for value in [
            "",
            "gzip, br, zstd",
            "gzip;q=0.000",
            "identity;q=1.000",
            "ZSTD;q=0, IDENTITY;Q=0.001",
            "*;q=0, identity;q=1",
        ] {
            headers.insert(
                hyper::header::ACCEPT_ENCODING,
                HeaderValue::from_str(value).unwrap(),
            );
            assert_eq!(require_identity(&headers), Ok(()), "{value}");
            assert_negotiated(&probe, headers.clone()).await;
        }
        for value in [
            "identity;q=0",
            "*;q=0",
            "zstd;q=1, identity;q=0",
            "identity;q=1, identity;q=0",
            "*;q=1, *;q=0",
        ] {
            headers.insert(
                hyper::header::ACCEPT_ENCODING,
                HeaderValue::from_str(value).unwrap(),
            );
            assert_eq!(
                require_identity(&headers),
                Err(PreflightError::IdentityRequired),
                "{value}"
            );
        }
        for value in [
            "identity;q=0.0000",
            "zstd;q=1.001",
            "gzip;Q =1",
            ";q=1",
            "gzip;q=-1",
            "identity;foo=1",
            "gzip;q=.5",
        ] {
            headers.insert(
                hyper::header::ACCEPT_ENCODING,
                HeaderValue::from_str(value).unwrap(),
            );
            assert_eq!(
                require_identity(&headers),
                Err(PreflightError::AcceptEncoding),
                "{value}"
            );
        }
        headers.insert(
            hyper::header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip"),
        );
        headers.append(
            hyper::header::ACCEPT_ENCODING,
            HeaderValue::from_static("*;q=0"),
        );
        assert_eq!(
            require_identity(&headers),
            Err(PreflightError::IdentityRequired)
        );
        headers.append(
            hyper::header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity;q=0.5"),
        );
        assert_eq!(require_identity(&headers), Ok(()));
        assert_negotiated(&probe, headers.clone()).await;
        headers.insert(
            hyper::header::ACCEPT_ENCODING,
            HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        assert_eq!(
            require_identity(&headers),
            Err(PreflightError::AcceptEncoding)
        );
    }
}
