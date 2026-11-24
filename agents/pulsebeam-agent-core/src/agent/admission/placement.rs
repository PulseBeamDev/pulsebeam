use alloc::{format, string::String};
use core::{fmt, num::NonZeroU16, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use pulsebeam_proto::NATIVE_QUIC_INITIAL_CID_BYTES as INITIAL_CID_BYTES;
use serde::{Deserialize, Serialize};

use super::{MAX_CAPABILITY_BODY_BYTES, valid_proof};
use crate::{Generation, HttpEffect, HttpMethod, HttpRequest, HttpResponse, OperationId};

const PROFILE: &str = "pulsebeam-rt-v1";
const MAX_CONTINUATION_BODY_BYTES: usize = 2_048;
const MAX_PLACEMENT_MS: u64 = 10_000;

#[derive(Debug, PartialEq, Eq)]
pub enum PlacementError {
    InvalidRequest,
    Status(u16),
    InvalidResponse,
    Expired,
}

pub struct PendingPlacement {
    generation: Generation,
    operation: OperationId,
    issued_at: Duration,
    continuing_owner: Option<String>,
}

pub struct Placed {
    pub generation: Generation,
    pub placement: Placement,
    pub deadline: Duration,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Placement {
    pub owner: String,
    pub initial_dcid: [u8; INITIAL_CID_BYTES],
    pub host: String,
    pub port: NonZeroU16,
    pub valid_for: Duration,
}

impl fmt::Debug for Placement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Placement")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("valid_for", &self.valid_for)
            .finish_non_exhaustive()
    }
}

/// Format the placement request for the configured HTTPS origin, not the advertised QUIC host.
/// The continuation receipt is already base64url-encoded without padding.
pub fn request(
    origin: &str,
    token: &str,
    room: Option<&str>,
    continuation: Option<(&str, &str)>,
) -> Result<HttpRequest, PlacementError> {
    #[derive(Serialize)]
    struct Request<'a> {
        profile: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        room: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        owner: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        successor_receipt: Option<&'a str>,
    }
    if !origin.starts_with("https://") {
        return Err(PlacementError::InvalidRequest);
    }
    if let Some((owner, receipt)) = continuation
        && (room.is_none()
            || !valid_proof(owner)
            || !(1..=1_024).contains(&receipt.len())
            || !receipt
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')))
    {
        return Err(PlacementError::InvalidRequest);
    }
    let body = serde_json::to_vec(&Request {
        profile: PROFILE,
        room,
        owner: continuation.map(|(owner, _)| owner),
        successor_receipt: continuation.map(|(_, receipt)| receipt),
    })
    .map_err(|_| PlacementError::InvalidRequest)?;
    let max = if continuation.is_some() {
        MAX_CONTINUATION_BODY_BYTES
    } else {
        MAX_CAPABILITY_BODY_BYTES
    };
    if body.len() > max {
        return Err(PlacementError::InvalidRequest);
    }
    Ok(HttpRequest {
        method: HttpMethod::Post,
        uri: format!("{origin}/api/v1/native/admissions"),
        headers: super::super::admission_headers(token),
        body,
    })
}

impl PendingPlacement {
    pub fn issue(
        generation: Generation,
        operation: OperationId,
        now: Duration,
        request: HttpRequest,
        continuing_owner: Option<&str>,
    ) -> (Self, HttpEffect) {
        (
            Self {
                generation,
                operation,
                issued_at: now,
                continuing_owner: continuing_owner.map(String::from),
            },
            HttpEffect::Request {
                operation,
                generation: Some(generation),
                request,
            },
        )
    }

    pub fn matches(&self, operation: OperationId) -> bool {
        self.operation == operation
    }

    pub fn cancel(self) -> HttpEffect {
        HttpEffect::Cancel {
            operation: self.operation,
        }
    }

    pub fn complete(
        self,
        now: Duration,
        response: &HttpResponse,
    ) -> Result<Placed, PlacementError> {
        let placement = Placement::decode(response)?;
        if self
            .continuing_owner
            .as_ref()
            .is_some_and(|owner| owner != &placement.owner)
        {
            return Err(PlacementError::InvalidResponse);
        }
        let deadline = self
            .issued_at
            .checked_add(placement.valid_for)
            .ok_or(PlacementError::Expired)?;
        if now >= deadline {
            return Err(PlacementError::Expired);
        }
        Ok(Placed {
            generation: self.generation,
            placement,
            deadline,
        })
    }
}

impl Placement {
    pub fn decode(response: &HttpResponse) -> Result<Self, PlacementError> {
        #[derive(Deserialize)]
        struct Response {
            profile: String,
            owner: String,
            initial_dcid: String,
            authority: Authority,
            valid_for_ms: u64,
        }
        #[derive(Deserialize)]
        struct Authority {
            host: String,
            port: NonZeroU16,
        }
        if response.status != 200 {
            return Err(PlacementError::Status(response.status));
        }
        if response.body.len() > MAX_CAPABILITY_BODY_BYTES {
            return Err(PlacementError::InvalidResponse);
        }
        let response: Response =
            serde_json::from_slice(&response.body).map_err(|_| PlacementError::InvalidResponse)?;
        if response.profile != PROFILE
            || !valid_proof(&response.owner)
            || !pulsebeam_proto::valid_native_quic_host(&response.authority.host)
            || !(1..=MAX_PLACEMENT_MS).contains(&response.valid_for_ms)
        {
            return Err(PlacementError::InvalidResponse);
        }
        let mut initial_dcid = [0; INITIAL_CID_BYTES];
        if URL_SAFE_NO_PAD.decode_slice(response.initial_dcid.as_bytes(), &mut initial_dcid)
            != Ok(INITIAL_CID_BYTES)
        {
            return Err(PlacementError::InvalidResponse);
        }
        Ok(Self {
            owner: response.owner,
            initial_dcid,
            host: response.authority.host,
            port: response.authority.port,
            valid_for: Duration::from_millis(response.valid_for_ms),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};

    #[test]
    fn placement_request_keeps_credentials_on_the_configured_origin() {
        let req = request(
            "https://api.example",
            "token",
            Some("stage"),
            Some(("owner", "dGVzdA")),
        )
        .unwrap();
        assert_eq!(req.uri, "https://api.example/api/v1/native/admissions");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"profile": PROFILE, "room": "stage", "owner": "owner", "successor_receipt": "dGVzdA"})
        );
        assert!(
            request(
                "https://api.example",
                "token",
                None,
                Some(("owner", "dGVzdA"))
            )
            .is_err()
        );
    }

    #[test]
    fn placement_correlates_the_http_operation_and_spends_elapsed_lease_time() {
        let mut ids = crate::id::IdGenerator::new();
        let generation = ids.generation();
        let operation = ids.operation();
        let issue = |owner| {
            PendingPlacement::issue(
                generation,
                operation,
                Duration::from_secs(5),
                request(
                    "https://api.example",
                    "token",
                    Some("stage"),
                    Some(("owner", "dGVzdA")),
                )
                .unwrap(),
                Some(owner),
            )
        };
        let (pending, effect) = issue("owner");
        assert!(pending.matches(operation));
        assert!(!pending.matches(ids.operation()));
        assert!(
            matches!(effect, HttpEffect::Request { operation: actual, generation: Some(current), .. } if current == generation && actual == operation)
        );
        let response = HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{"profile":"pulsebeam-rt-v1","owner":"owner","initial_dcid":"AAAAAAAAAAAAAAAAAAAAAAAAAAA","authority":{"host":"edge.example","port":443},"valid_for_ms":2000}"#.to_vec(),
        };
        let placed = pending.complete(Duration::from_secs(6), &response).unwrap();
        assert_eq!(placed.generation, generation);
        assert_eq!(placed.deadline, Duration::from_secs(7));
        assert!(matches!(
            issue("owner").0.complete(Duration::from_secs(7), &response),
            Err(PlacementError::Expired)
        ));
        assert!(matches!(
            issue("other").0.complete(Duration::from_secs(6), &response),
            Err(PlacementError::InvalidResponse)
        ));
    }

    #[test]
    fn placement_response_requires_bounded_proof_tls_authority_and_lease() {
        let response = |profile, owner, host, port: u32, valid_for_ms: u64| {
            HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&serde_json::json!({"profile": profile, "owner": owner, "initial_dcid":"AAAAAAAAAAAAAAAAAAAAAAAAAAA", "authority": {"host": host, "port": port}, "valid_for_ms": valid_for_ms})).unwrap(),
        }
        };
        let valid = response(PROFILE, "owner", "edge.example", 443, 10_000);
        let placement = Placement::decode(&valid).unwrap();
        assert_eq!(placement.port.get(), 443);
        for invalid in [
            response("rtc", "owner", "edge.example", 443, 10_000),
            response(PROFILE, "owner", "127.0.0.1", 443, 10_000),
            response(PROFILE, "owner", "edge.example/path", 443, 10_000),
            response(PROFILE, "owner", "edge.example", 0, 10_000),
            response(PROFILE, "owner", "edge.example", 443, 10_001),
            response(PROFILE, "", "edge.example", 443, 10_000),
            HttpResponse {
                body: vec![b' '; 1_025],
                ..valid
            },
        ] {
            assert_eq!(
                Placement::decode(&invalid).unwrap_err(),
                PlacementError::InvalidResponse
            );
        }
    }
}
