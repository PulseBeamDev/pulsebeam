use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

use serde::Deserialize;

use super::Credential;
use crate::{
    Generation, HttpEffect, HttpHeader, HttpMethod, HttpRequest, HttpResponse, OperationId,
};

const MAX_CAPABILITY_BODY_BYTES: usize = 1_024;
const MAX_PROOF_BYTES: usize = 256;
pub(super) const MAX_CREATE_BODY_BYTES: usize = 262_144;
const OWNER_HEADER: &str = "PulseBeam-Owner";
const ATTEMPT_HEADER: &str = "PulseBeam-Attempt";

pub mod placement;

fn valid_proof(proof: &str) -> bool {
    !proof.is_empty()
        && proof.len() <= MAX_PROOF_BYTES
        && proof.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

pub(super) fn attempt_unavailable(response: &HttpResponse) -> bool {
    #[derive(Deserialize)]
    struct Problem<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
    }
    response.status == 410
        && response.body.len() <= MAX_CAPABILITY_BODY_BYTES
        && serde_json::from_slice::<Problem<'_>>(&response.body).is_ok_and(|problem| {
            problem.kind == pulsebeam_proto::state::ATTEMPT_UNAVAILABLE_PROBLEM
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    InvalidCapability,
    CapabilityUnavailable,
    OwnershipClaimed,
    SequenceExhausted,
    InvalidRequest,
    RequestPending,
    RetryExhausted,
    RequestAccepted,
}

#[derive(Default)]
pub(super) struct Ownership {
    proof: Option<String>,
    sequence: u64,
    latest_generation: Option<Generation>,
}

impl core::fmt::Debug for Ownership {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("Ownership([REDACTED])")
    }
}

impl Ownership {
    pub(super) fn available(&self) -> bool {
        self.proof.is_some()
    }

    pub(super) fn install(&mut self, body: &[u8]) -> Result<(), Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Capability {
            owner: String,
        }

        if self.sequence != 0 {
            return Err(Error::OwnershipClaimed);
        }
        if body.len() > MAX_CAPABILITY_BODY_BYTES {
            return Err(Error::InvalidCapability);
        }
        let capability: Capability =
            serde_json::from_slice(body).map_err(|_| Error::InvalidCapability)?;
        if !valid_proof(&capability.owner) {
            return Err(Error::InvalidCapability);
        }
        self.proof = Some(capability.owner);
        Ok(())
    }

    pub(super) fn create(
        &mut self,
        operation: OperationId,
        credential: Credential,
        generation: Generation,
        mut request: HttpRequest,
        maximum_attempts: u8,
    ) -> Result<(CreateRequest, HttpEffect), Error> {
        let proof = self.proof.as_ref().ok_or(Error::CapabilityUnavailable)?;
        if request.method != HttpMethod::Post
            || request.headers.iter().any(|header| {
                header.name.eq_ignore_ascii_case(OWNER_HEADER)
                    || header.name.eq_ignore_ascii_case(ATTEMPT_HEADER)
            })
        {
            return Err(Error::InvalidRequest);
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        request.headers.push(HttpHeader {
            name: String::from(OWNER_HEADER),
            value: proof.clone(),
        });
        request.headers.push(HttpHeader {
            name: String::from(ATTEMPT_HEADER),
            value: sequence.to_string(),
        });
        let (captured, effect) =
            CreateRequest::capture(operation, credential, generation, request, maximum_attempts)?;
        self.sequence = sequence;
        self.latest_generation = Some(generation);
        Ok((captured, effect))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReceiptState {
    Pending,
    Uncertain,
    Complete,
}

struct Receipt {
    operation: OperationId,
    state: ReceiptState,
}

pub(super) struct AcceptedReceipts {
    generation: Generation,
    operations: Vec<OperationId>,
}

impl AcceptedReceipts {
    pub(super) fn is_latest_receipt(&self, operation: OperationId, ownership: &Ownership) -> bool {
        ownership.latest_generation == Some(self.generation) && self.operations.contains(&operation)
    }
}

pub(super) struct CreateRequest {
    pub(super) operation: OperationId,
    pub(super) credential: Credential,
    generation: Generation,
    request: HttpRequest,
    maximum_attempts: u8,
    receipts: Vec<Receipt>,
    uncertain: bool,
    accepted: bool,
}

impl core::fmt::Debug for CreateRequest {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("CreateRequest([REDACTED])")
    }
}

impl CreateRequest {
    pub(super) fn capture(
        operation: OperationId,
        credential: Credential,
        generation: Generation,
        request: HttpRequest,
        maximum_attempts: u8,
    ) -> Result<(Self, HttpEffect), Error> {
        if maximum_attempts == 0 || request.body.len() > MAX_CREATE_BODY_BYTES {
            return Err(Error::InvalidRequest);
        }
        let captured = Self {
            operation,
            credential,
            generation,
            request,
            maximum_attempts,
            receipts: vec![Receipt {
                operation,
                state: ReceiptState::Pending,
            }],
            uncertain: false,
            accepted: false,
        };
        let effect = captured.effect();
        Ok((captured, effect))
    }

    fn effect(&self) -> HttpEffect {
        HttpEffect::Request {
            operation: self.operation,
            generation: Some(self.generation),
            request: self.request.clone(),
        }
    }

    pub(super) fn failed(&mut self, operation: OperationId) -> bool {
        let Some(receipt) = self.receipts.iter_mut().find(|receipt| {
            receipt.operation == operation && receipt.state == ReceiptState::Pending
        }) else {
            return false;
        };
        receipt.state = ReceiptState::Uncertain;
        self.uncertain = true;
        operation == self.operation
    }

    pub(super) fn responded(&mut self, operation: OperationId, uncertain: bool) -> Option<bool> {
        let receipt = self.receipts.iter_mut().find(|receipt| {
            receipt.operation == operation && receipt.state != ReceiptState::Complete
        })?;
        receipt.state = ReceiptState::Complete;
        self.uncertain |= uncertain;
        Some(self.uncertain)
    }

    pub(super) fn retry(&mut self, operation: OperationId) -> Result<HttpEffect, Error> {
        if self.accepted {
            return Err(Error::RequestAccepted);
        }
        if self
            .receipts
            .iter()
            .any(|receipt| receipt.operation == operation)
        {
            return Err(Error::InvalidRequest);
        }
        if self
            .receipts
            .last()
            .is_some_and(|receipt| receipt.state == ReceiptState::Pending)
        {
            return Err(Error::RequestPending);
        }
        if self.receipts.len() >= usize::from(self.maximum_attempts) {
            return Err(Error::RetryExhausted);
        }
        self.receipts.push(Receipt {
            operation,
            state: ReceiptState::Pending,
        });
        self.operation = operation;
        Ok(self.effect())
    }

    pub(super) fn is_latest_receipt(&self, operation: OperationId, ownership: &Ownership) -> bool {
        ownership.latest_generation == Some(self.generation)
            && self
                .receipts
                .iter()
                .any(|receipt| receipt.operation == operation)
    }

    pub(super) fn into_accepted_receipts(self) -> AcceptedReceipts {
        AcceptedReceipts {
            generation: self.generation,
            operations: self
                .receipts
                .into_iter()
                .map(|receipt| receipt.operation)
                .collect(),
        }
    }

    pub(super) fn can_retry(&self) -> bool {
        !self.accepted && self.receipts.len() < usize::from(self.maximum_attempts)
    }

    pub(super) fn unsettled(&self) -> impl Iterator<Item = (OperationId, bool)> + '_ {
        self.receipts.iter().filter_map(|receipt| {
            (receipt.state != ReceiptState::Complete)
                .then_some((receipt.operation, receipt.state == ReceiptState::Pending))
        })
    }

    pub(super) fn accept(&mut self) -> Vec<OperationId> {
        let pending = self
            .receipts
            .iter()
            .filter_map(|receipt| {
                (receipt.state == ReceiptState::Pending).then_some(receipt.operation)
            })
            .collect();
        for receipt in &mut self.receipts {
            receipt.state = ReceiptState::Complete;
        }
        self.accepted = true;
        pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::IdGenerator;

    fn credential() -> Credential {
        Credential {
            token: String::from("captured-secret"),
            revision: 7,
        }
    }

    fn request() -> HttpRequest {
        HttpRequest {
            method: HttpMethod::Post,
            uri: String::from("https://example.test/api/v1/native"),
            headers: vec![HttpHeader {
                name: String::from("Authorization"),
                value: String::from("Bearer captured-secret"),
            }],
            body:
                br#"{ "profile":"pulsebeam.media.v1", "room":"selected", "offer":"original\r\n" }"#
                    .to_vec(),
        }
    }

    fn frame(effect: HttpEffect, operation: OperationId, generation: Generation) -> HttpRequest {
        let HttpEffect::Request {
            operation: actual,
            generation: peer,
            request,
        } = effect
        else {
            panic!("expected an HTTP request");
        };
        assert_eq!(actual, operation);
        assert_eq!(peer, Some(generation));
        request
    }

    #[test]
    fn ownership_and_exact_request_receipts_do_not_escape_into_fresh_admission() {
        let mut ids = IdGenerator::new();
        let generation = ids.generation();
        let first = ids.operation();
        let second = ids.operation();
        let third = ids.operation();
        let mut ownership = Ownership::default();
        assert!(matches!(
            ownership.create(first, credential(), generation, request(), 3),
            Err(Error::CapabilityUnavailable)
        ));
        for body in [
            b"[]".as_slice(),
            br#"{"owner":""}"#,
            br#"{"owner":"secret","extra":true}"#,
            br#"{"owner":"one","owner":"two"}"#,
            br#"{"owner":"bad\r\nheader"}"#,
            br#"{"owner":"space forbidden"}"#,
        ] {
            assert_eq!(ownership.install(body), Err(Error::InvalidCapability));
        }
        assert_eq!(
            ownership.install(&vec![b' '; MAX_CAPABILITY_BODY_BYTES + 1]),
            Err(Error::InvalidCapability)
        );
        let oversized = serde_json::json!({"owner": "x".repeat(MAX_PROOF_BYTES + 1)});
        assert_eq!(
            ownership.install(&serde_json::to_vec(&oversized).unwrap()),
            Err(Error::InvalidCapability)
        );
        ownership
            .install(br#"{"owner":"unused-opaque-proof"}"#)
            .unwrap();
        ownership
            .install(br#"{"owner":"captured-opaque-proof"}"#)
            .unwrap();
        assert_eq!(format!("{ownership:?}"), "Ownership([REDACTED])");

        let mut invalid = request();
        invalid.headers.push(HttpHeader {
            name: String::from("pulsebeam-owner"),
            value: String::from("injected"),
        });
        assert!(matches!(
            ownership.create(first, credential(), generation, invalid, 3),
            Err(Error::InvalidRequest)
        ));
        let mut oversized = request();
        oversized.body = vec![0; MAX_CREATE_BODY_BYTES + 1];
        assert!(matches!(
            ownership.create(first, credential(), generation, oversized, 3),
            Err(Error::InvalidRequest)
        ));
        assert!(matches!(
            ownership.create(first, credential(), generation, request(), 0),
            Err(Error::InvalidRequest)
        ));
        assert_eq!(ownership.sequence, 0);

        let mut original = request();
        let (mut issued, initial) = ownership
            .create(first, credential(), generation, original.clone(), 3)
            .unwrap();
        original.body.fill(b'x');
        original.headers[0].value = String::from("Bearer latest-secret");
        let captured = frame(initial, first, generation);
        assert_eq!(captured.body, request().body);
        assert_eq!(captured.headers[0].value, "Bearer captured-secret");
        assert_eq!(captured.headers[1].value, "captured-opaque-proof");
        assert_eq!(captured.headers[2].value, "1");
        assert_eq!(issued.credential.token, "captured-secret");
        assert_eq!(issued.credential.revision, 7);
        assert_eq!(format!("{issued:?}"), "CreateRequest([REDACTED])");
        assert_eq!(
            ownership.install(br#"{"owner":"new-proof"}"#),
            Err(Error::OwnershipClaimed)
        );
        assert_eq!(issued.retry(second), Err(Error::RequestPending));
        assert!(!issued.failed(third));
        assert!(issued.failed(first));
        assert!(!issued.failed(first));
        assert_eq!(issued.retry(first), Err(Error::InvalidRequest));
        assert_eq!(
            frame(issued.retry(second).unwrap(), second, generation),
            captured
        );
        assert_eq!(issued.responded(first, false), Some(true));
        assert_eq!(issued.retry(third), Err(Error::RequestPending));
        assert_eq!(issued.accept(), vec![second]);
        assert_eq!(issued.responded(second, false), None);
        assert!(!issued.failed(second));
        assert_eq!(issued.retry(third), Err(Error::RequestAccepted));

        let next_generation = ids.generation();
        let fourth = ids.operation();
        let mut desired = request();
        desired.headers[0].value = String::from("Bearer latest-secret");
        desired.body = br#"{"profile":"pulsebeam.media.v1","room":"next","offer":"next"}"#.to_vec();
        let (mut next, initial) = ownership
            .create(
                third,
                Credential {
                    token: String::from("latest-secret"),
                    revision: 8,
                },
                next_generation,
                desired.clone(),
                2,
            )
            .unwrap();
        let next_frame = frame(initial, third, next_generation);
        assert_eq!(next_frame.body, desired.body);
        assert_eq!(next.credential.revision, 8);
        assert_eq!(next_frame.headers[0].value, "Bearer latest-secret");
        assert_eq!(next_frame.headers[1].value, captured.headers[1].value);
        assert_eq!(next_frame.headers[2].value, "2");
        assert_eq!(next.responded(third, false), Some(false));
        assert_eq!(
            frame(next.retry(fourth).unwrap(), fourth, next_generation),
            next_frame
        );
        assert!(next.failed(fourth));
        assert_eq!(next.retry(ids.operation()), Err(Error::RetryExhausted));
        assert_eq!(next.responded(fourth, false), Some(true));
        assert_eq!(
            ownership.install(br#"{"owner":"reset-proof"}"#),
            Err(Error::OwnershipClaimed)
        );
        ownership.sequence = u64::MAX;
        assert!(matches!(
            ownership.create(ids.operation(), credential(), next_generation, request(), 2),
            Err(Error::SequenceExhausted)
        ));
        assert_eq!(ownership.sequence, u64::MAX);
    }
}
