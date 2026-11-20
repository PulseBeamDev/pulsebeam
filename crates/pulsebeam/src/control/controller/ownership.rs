use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap},
    fmt,
    num::NonZeroU64,
    time::Duration,
};

use arrayvec::ArrayVec;
use pulsebeam_core::auth::{VerifiedAuthorization, VerifiedPrincipal};
use pulsebeam_runtime::{rand::RngCore, time::Instant};
use sha3::{Digest, Sha3_256};

mod result;

use super::SignalingProtocol;
use crate::{
    entity::{ConnectionId, ParticipantId, ProjectId, RoomId},
    route::NodeTransportAddress,
};
pub(crate) use result::ResultBytes;
use result::{CachedResult, ResultBudget, ResultReservation};

const FRESH_PER_PRINCIPAL: usize = 4;
const FRESH_LIFETIME: Duration = Duration::from_secs(60);
const RESULT_LIFETIME: Duration = Duration::from_secs(60);
const IDLE_LIFETIME: Duration = Duration::from_secs(300);
pub(crate) const MAX_RESULT_BYTES: usize = 1_048_576;
pub(super) const MAX_WAITERS: usize = 4;

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct PrincipalKey {
    project: ProjectId,
    participant: ParticipantId,
    roomless_attempt: Option<Proof>,
}

impl PrincipalKey {
    pub(crate) fn from_principal(principal: &VerifiedPrincipal) -> Self {
        Self {
            project: principal.project_id(),
            participant: principal.participant_id().clone(),
            roomless_attempt: None,
        }
    }

    pub(crate) fn from_verified(authorization: &VerifiedAuthorization) -> Self {
        Self {
            project: authorization.project_id,
            participant: authorization.participant_id.clone(),
            roomless_attempt: None,
        }
    }

    pub(super) fn roomless(mut self, proof: Proof) -> Self {
        self.roomless_attempt = Some(proof);
        self
    }
}

impl Ord for PrincipalKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.project
            .cmp(&other.project)
            .then_with(|| self.participant.as_str().cmp(other.participant.as_str()))
            .then_with(|| self.roomless_attempt.cmp(&other.roomless_attempt))
    }
}

impl PartialOrd for PrincipalKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for PrincipalKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PrincipalKey([redacted])")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Proof([u8; 32]);

impl Proof {
    pub(crate) fn encode(self) -> String {
        base32::encode(base32::Alphabet::Crockford, &self.0)
    }

    pub(crate) fn parse(encoded: &str) -> Result<Self, Error> {
        if encoded.len() != 52 {
            return Err(Error::InvalidProof);
        }
        let bytes = base32::decode(base32::Alphabet::Crockford, encoded)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(Error::InvalidProof)?;
        let proof = Self(bytes);
        if proof.encode() != encoded {
            return Err(Error::InvalidProof);
        }
        Ok(proof)
    }
}

impl fmt::Debug for Proof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Proof([redacted])")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestBinding([u8; 32]);

impl RequestBinding {
    pub(crate) fn for_request<'a>(
        protocol: SignalingProtocol,
        room: impl Into<Option<&'a RoomId>>,
        offer: &str,
        credential: &str,
    ) -> Result<Self, Error> {
        let protocol = [match protocol {
            SignalingProtocol::State => 1,
            SignalingProtocol::Whip => 2,
            SignalingProtocol::Whep => 3,
        }];
        let mut digest = Sha3_256::new();
        digest.update(b"pulsebeam.admission.request.v1");
        for part in [
            protocol.as_slice(),
            room.into().map_or(&[][..], |room| room.as_str().as_bytes()),
            offer.as_bytes(),
            credential.as_bytes(),
        ] {
            let length = u64::try_from(part.len()).map_err(|_| Error::Capacity)?;
            digest.update(length.to_be_bytes());
            digest.update(part);
        }
        Ok(Self(digest.finalize().into()))
    }
}

impl fmt::Debug for RequestBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RequestBinding([redacted])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Ticket {
    principal: PrincipalKey,
    proof: Proof,
    sequence: NonZeroU64,
}

impl Ticket {
    pub(crate) fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.principal.participant.as_str().len())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    Start(Ticket),
    Pending(Ticket),
    Replay(Ticket),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    InvalidProof,
    UnknownProof,
    Replaced,
    StaleAttempt,
    ChangedAttempt,
    Capacity,
    ReplayUnavailable,
    QuiescencePending,
    ResultTooLarge,
    InvalidTransition,
}

pub(crate) struct Limits {
    records: usize,
    fresh: usize,
    in_flight: usize,
    result_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            records: 4096,
            fresh: 1024,
            in_flight: 64,
            result_bytes: 64 * MAX_RESULT_BYTES,
        }
    }
}

struct Fresh {
    proof: Proof,
    base: Option<Proof>,
    deadline: Instant,
}

struct Family {
    proof: Proof,
    sequence: NonZeroU64,
    binding: RequestBinding,
    pending: bool,
    retryable_materialization: bool,
    result: Option<CachedResult>,
    idle: Option<Instant>,
    expired: bool,
}

#[derive(Default)]
struct Record {
    generation: Option<Proof>,
    fresh: ArrayVec<Fresh, FRESH_PER_PRINCIPAL>,
    family: Option<Family>,
    pins: usize,
    deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Incarnation {
    pub participant: ParticipantId,
    pub connection: ConnectionId,
    pub transport: NodeTransportAddress,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Retirement {
    Live,
    Queued,
    InFlight,
    Failed,
    Applied,
}

struct Physical {
    incarnation: Incarnation,
    submitted: bool,
    retirement: Retirement,
}

struct Work {
    pending: bool,
    waiters: usize,
    physical: Option<Physical>,
    result_reservation: Option<ResultReservation>,
}

pub(crate) struct Authority {
    limits: Limits,
    records: HashMap<PrincipalKey, Record>,
    deadlines: BTreeSet<(Instant, PrincipalKey)>,
    work: HashMap<Ticket, Work>,
    incarnations: HashMap<Incarnation, Ticket>,
    retirements: BTreeSet<Ticket>,
    fresh: usize,
    in_flight: usize,
    result_bytes: usize,
    result_budget: ResultBudget,
}

impl Default for Authority {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl Authority {
    fn new(limits: Limits) -> Self {
        let result_budget = ResultBudget::new(limits.result_bytes);
        Self {
            limits,
            records: HashMap::new(),
            deadlines: BTreeSet::new(),
            work: HashMap::new(),
            incarnations: HashMap::new(),
            retirements: BTreeSet::new(),
            fresh: 0,
            in_flight: 0,
            result_bytes: 0,
            result_budget,
        }
    }

    // The caller supplies runtime OS-seeded entropy, never request-derived randomness.
    pub(crate) fn issue(
        &mut self,
        principal: PrincipalKey,
        now: Instant,
        entropy: &mut (impl RngCore + rand::CryptoRng),
    ) -> Result<Proof, Error> {
        self.expire_principal(&principal, now);
        let record = self.records.get(&principal);
        if self.fresh >= self.limits.fresh
            || record.is_some_and(|record| record.fresh.is_full())
            || record.is_none() && self.records.len() >= self.limits.records
        {
            return Err(Error::Capacity);
        }
        let deadline = now.checked_add(FRESH_LIFETIME).ok_or(Error::Capacity)?;
        let proof = (0..4)
            .find_map(|_| {
                let mut bytes = [0; 32];
                entropy.fill_bytes(&mut bytes);
                let proof = Proof(bytes);
                record
                    .is_none_or(|record| {
                        record.generation != Some(proof)
                            && !record.fresh.iter().any(|fresh| fresh.proof == proof)
                    })
                    .then_some(proof)
            })
            .ok_or(Error::Capacity)?;
        let mut record = self.take_record(&principal).unwrap_or_default();
        record.fresh.push(Fresh {
            proof,
            base: record.generation,
            deadline,
        });
        add_credit(&mut self.fresh, 1);
        self.put_record(principal, record);
        Ok(proof)
    }

    pub(super) fn issue_roomless(
        &mut self,
        principal: PrincipalKey,
        now: Instant,
        entropy: &mut (impl RngCore + rand::CryptoRng),
    ) -> Result<Proof, Error> {
        if self.fresh >= self.limits.fresh || self.records.len() >= self.limits.records {
            return Err(Error::Capacity);
        }
        let deadline = now.checked_add(FRESH_LIFETIME).ok_or(Error::Capacity)?;
        for _ in 0..4 {
            let mut bytes = [0; 32];
            entropy.fill_bytes(&mut bytes);
            let proof = Proof(bytes);
            let key = principal.clone().roomless(proof);
            if self.records.contains_key(&key) {
                continue;
            }
            let mut record = Record::default();
            record.fresh.push(Fresh {
                proof,
                base: None,
                deadline,
            });
            add_credit(&mut self.fresh, 1);
            self.put_record(key, record);
            return Ok(proof);
        }
        Err(Error::Capacity)
    }

    pub(crate) fn preview(
        &mut self,
        principal: &PrincipalKey,
        proof: Proof,
        sequence: NonZeroU64,
        binding: RequestBinding,
        now: Instant,
    ) -> Result<Admission, Error> {
        self.expire_principal(principal, now);
        let record = self.records.get(principal).ok_or(Error::UnknownProof)?;
        let ticket = Ticket {
            principal: principal.clone(),
            proof,
            sequence,
        };
        let continuation = record
            .family
            .as_ref()
            .is_some_and(|family| family.proof == proof && !family.expired);
        if continuation {
            let family = record.family.as_ref().ok_or(Error::InvalidTransition)?;
            match sequence.cmp(&family.sequence) {
                Ordering::Less => return Err(Error::StaleAttempt),
                Ordering::Equal => {
                    if family.binding != binding {
                        return Err(Error::ChangedAttempt);
                    }
                    if family.pending {
                        if family.retryable_materialization {
                            return Err(Error::QuiescencePending);
                        }
                        let work = self.work.get(&ticket).ok_or(Error::ReplayUnavailable)?;
                        if work.waiters >= MAX_WAITERS {
                            return Err(Error::Capacity);
                        }
                        return Ok(Admission::Pending(ticket));
                    }
                    if family.result.is_some() {
                        return Ok(Admission::Replay(ticket));
                    }
                    if !family.retryable_materialization {
                        return Err(Error::ReplayUnavailable);
                    }
                    // Only an uncommitted materialization failure can repeat this request.
                    // Physical retirement must clear pending before allocation is retried.
                }
                Ordering::Greater => {}
            }
        } else if !record
            .fresh
            .iter()
            .any(|fresh| fresh.proof == proof && fresh.base == record.generation)
        {
            return Err(if record.generation.is_some() {
                Error::Replaced
            } else {
                Error::UnknownProof
            });
        }
        self.admission_bytes(record)?;
        Ok(Admission::Start(ticket))
    }

    fn admission_bytes(&self, record: &Record) -> Result<usize, Error> {
        let cached = record
            .family
            .as_ref()
            .and_then(|family| family.result.as_ref())
            .map_or(0, |result| result.bytes.len());
        let bytes = self
            .result_bytes
            .checked_sub(cached)
            .and_then(|bytes| bytes.checked_add(MAX_RESULT_BYTES))
            .filter(|bytes| *bytes <= self.limits.result_bytes)
            .ok_or(Error::Capacity)?;
        if self.in_flight >= self.limits.in_flight
            || !self.result_budget.can_reserve(
                record
                    .family
                    .as_ref()
                    .and_then(|family| family.result.as_ref()),
            )
        {
            return Err(Error::Capacity);
        }
        Ok(bytes)
    }

    pub(crate) fn reserve_binding(&mut self) -> Result<(), Error> {
        self.work.try_reserve(1).map_err(|_| Error::Capacity)?;
        self.incarnations
            .try_reserve(1)
            .map_err(|_| Error::Capacity)
    }

    pub(crate) fn begin(
        &mut self,
        principal: &PrincipalKey,
        proof: Proof,
        sequence: NonZeroU64,
        binding: RequestBinding,
        now: Instant,
    ) -> Result<Admission, Error> {
        let admission = self.preview(principal, proof, sequence, binding, now)?;
        let Admission::Start(ticket) = admission else {
            if let Admission::Pending(ref ticket) = admission {
                let work = self.work.get_mut(ticket).ok_or(Error::ReplayUnavailable)?;
                work.waiters = work.waiters.checked_add(1).ok_or(Error::Capacity)?;
            }
            return Ok(admission);
        };
        let record = self.records.get(principal).ok_or(Error::UnknownProof)?;
        let bytes = self.admission_bytes(record)?;
        let continuation = record
            .family
            .as_ref()
            .is_some_and(|family| family.proof == proof && !family.expired);
        self.work.try_reserve(1).map_err(|_| Error::Capacity)?;
        let record = self.records.get_mut(principal).ok_or(Error::UnknownProof)?;
        let reservation = self
            .result_budget
            .reserve(record.family.as_mut().map(|family| &mut family.result))?;
        let mut record = self.take_record(principal).ok_or(Error::UnknownProof)?;
        if !continuation {
            release_credit(&mut self.fresh, record.fresh.len());
            record.fresh.clear();
            record.generation = Some(proof);
        }
        record.family = Some(Family {
            proof,
            sequence,
            binding,
            pending: true,
            retryable_materialization: false,
            result: None,
            idle: None,
            expired: false,
        });
        add_credit(&mut record.pins, 1);
        self.work.insert(
            ticket.clone(),
            Work {
                pending: true,
                waiters: 1,
                physical: None,
                result_reservation: Some(reservation),
            },
        );
        add_credit(&mut self.in_flight, 1);
        self.result_bytes = bytes;
        self.put_record(principal.clone(), record);
        Ok(Admission::Start(ticket))
    }

    pub(crate) fn retry_failed_materialization(&mut self, ticket: &Ticket) {
        if !self.work.get(ticket).is_some_and(|work| work.pending) {
            return;
        }
        if let Some(family) = self
            .records
            .get_mut(&ticket.principal)
            .and_then(|record| record.family.as_mut())
            && family.proof == ticket.proof
            && family.sequence == ticket.sequence
            && family.pending
        {
            family.retryable_materialization = true;
        }
    }

    pub(crate) fn detach_waiter(&mut self, ticket: &Ticket) {
        if let Some(work) = self.work.get_mut(ticket) {
            work.waiters = work.waiters.saturating_sub(1);
        }
    }

    pub(crate) fn commit(
        &mut self,
        ticket: &Ticket,
        bytes: Vec<u8>,
        now: Instant,
    ) -> Result<(), Error> {
        self.expire_principal(&ticket.principal, now);
        self.family(ticket)?;
        let record = self
            .records
            .get(&ticket.principal)
            .ok_or(Error::UnknownProof)?;
        if !self.work.get(ticket).is_some_and(|work| {
            work.pending
                && work.physical.as_ref().is_none_or(|physical| {
                    physical.submitted && physical.retirement == Retirement::Live
                })
        }) {
            return Err(Error::InvalidTransition);
        }
        if record.pins != 1 {
            return Err(Error::QuiescencePending);
        }
        if bytes.len() > MAX_RESULT_BYTES {
            return Err(Error::ResultTooLarge);
        }
        let deadline = now.checked_add(RESULT_LIFETIME).ok_or(Error::Capacity)?;
        let size = bytes.len();
        let reservation = self
            .work
            .get_mut(ticket)
            .ok_or(Error::InvalidTransition)?
            .result_reservation
            .take()
            .ok_or(Error::InvalidTransition)?;
        let bytes = reservation.finish(bytes);
        let record = self
            .records
            .get_mut(&ticket.principal)
            .ok_or(Error::UnknownProof)?;
        let family = record.family.as_mut().ok_or(Error::InvalidTransition)?;
        family.pending = false;
        family.result = Some(CachedResult { bytes, deadline });
        self.work
            .get_mut(ticket)
            .ok_or(Error::InvalidTransition)?
            .pending = false;
        release_credit(&mut self.in_flight, 1);
        release_credit(&mut self.result_bytes, MAX_RESULT_BYTES);
        add_credit(&mut self.result_bytes, size);
        self.reschedule(&ticket.principal);
        Ok(())
    }

    pub(crate) fn result(&mut self, ticket: &Ticket, now: Instant) -> Result<ResultBytes, Error> {
        self.expire_principal(&ticket.principal, now);
        self.family(ticket)?
            .result
            .as_ref()
            .map(|result| result.bytes.clone())
            .ok_or(Error::ReplayUnavailable)
    }

    pub(crate) fn is_current(&self, ticket: &Ticket) -> bool {
        self.family(ticket).is_ok() && self.work.get(ticket).is_some_and(|work| !work.pending)
    }

    pub(crate) fn bind(
        &mut self,
        ticket: &Ticket,
        authorization: &VerifiedAuthorization,
        incarnation: Incarnation,
    ) -> Result<(), Error> {
        self.family(ticket)?;
        let principal = PrincipalKey::from_verified(authorization);
        let principal = if authorization.room_id.is_none() {
            principal.roomless(ticket.proof)
        } else {
            principal
        };
        if ticket.principal != principal
            || incarnation.participant != authorization.participant_id
            || self.incarnations.contains_key(&incarnation)
            || !self
                .work
                .get(ticket)
                .is_some_and(|work| work.pending && work.physical.is_none())
        {
            return Err(Error::InvalidTransition);
        }
        self.incarnations
            .try_reserve(1)
            .map_err(|_| Error::Capacity)?;
        self.work
            .get_mut(ticket)
            .ok_or(Error::InvalidTransition)?
            .physical = Some(Physical {
            incarnation,
            submitted: false,
            retirement: Retirement::Live,
        });
        self.incarnations.insert(incarnation, ticket.clone());
        Ok(())
    }

    pub(crate) fn predecessors(&self, ticket: &Ticket) -> Vec<Incarnation> {
        self.work
            .iter()
            .filter_map(|(older, work)| {
                (older.principal == ticket.principal && older != ticket)
                    .then(|| work.physical.as_ref().map(|physical| physical.incarnation))
                    .flatten()
            })
            .collect()
    }

    pub(crate) fn retry_retirement(&mut self, incarnation: Incarnation) {
        let failed = self
            .incarnations
            .get(&incarnation)
            .and_then(|ticket| self.work.get(ticket))
            .and_then(|work| work.physical.as_ref())
            .is_some_and(|physical| physical.retirement == Retirement::Failed);
        if failed {
            self.queue_retirement(incarnation);
        }
    }

    pub(crate) fn has_incarnation(&self, incarnation: Incarnation) -> bool {
        self.incarnations.contains_key(&incarnation)
    }

    pub(crate) fn submit(&mut self, incarnation: Incarnation) -> Result<(), Error> {
        let Some(ticket) = self.incarnations.get(&incarnation) else {
            return Ok(());
        };
        self.family(ticket)?;
        if self
            .records
            .get(&ticket.principal)
            .ok_or(Error::UnknownProof)?
            .pins
            != 1
        {
            return Err(Error::QuiescencePending);
        }
        let work = self.work.get_mut(ticket).ok_or(Error::InvalidTransition)?;
        if !work.pending {
            return Err(Error::InvalidTransition);
        }
        let physical = work.physical.as_mut().ok_or(Error::InvalidTransition)?;
        if physical.submitted || physical.retirement != Retirement::Live {
            return Err(Error::InvalidTransition);
        }
        physical.submitted = true;
        Ok(())
    }

    pub(crate) fn cancel_unsubmitted(&mut self, incarnation: Incarnation, now: Instant) {
        let Some(ticket) = self.incarnations.get(&incarnation).cloned() else {
            return;
        };
        if self
            .work
            .get(&ticket)
            .and_then(|work| work.physical.as_ref())
            .is_some_and(|physical| !physical.submitted)
        {
            self.retire(&ticket, now);
        }
    }

    // Call only after the last command is consumed and final cleanup is published.
    pub(crate) fn queue_retirement(&mut self, incarnation: Incarnation) {
        let Some(ticket) = self.incarnations.get(&incarnation) else {
            return;
        };
        let Some(physical) = self
            .work
            .get_mut(ticket)
            .and_then(|work| work.physical.as_mut())
        else {
            pulsebeam_runtime::fatal!("ownership incarnation index lost its work");
        };
        if physical.submitted
            && matches!(physical.retirement, Retirement::Live | Retirement::Failed)
        {
            physical.retirement = Retirement::Queued;
            self.retirements.insert(ticket.clone());
        }
    }

    pub(crate) fn next_retirement(&self) -> Option<(Ticket, Incarnation)> {
        let ticket = self.retirements.first()?;
        let physical = self.work.get(ticket)?.physical.as_ref()?;
        Some((ticket.clone(), physical.incarnation))
    }

    pub(crate) fn start_retirement(&mut self, ticket: &Ticket) {
        if !self.retirements.remove(ticket) {
            pulsebeam_runtime::fatal!("ownership retirement was not queued");
        }
        let Some(physical) = self
            .work
            .get_mut(ticket)
            .and_then(|work| work.physical.as_mut())
        else {
            pulsebeam_runtime::fatal!("ownership retirement lost its incarnation");
        };
        if physical.retirement != Retirement::Queued {
            pulsebeam_runtime::fatal!("ownership retirement was started twice");
        }
        physical.retirement = Retirement::InFlight;
    }

    pub(crate) fn finish_retirement(
        &mut self,
        ticket: &Ticket,
        incarnation: Incarnation,
        success: bool,
        now: Instant,
    ) {
        let Some(physical) = self
            .work
            .get_mut(ticket)
            .and_then(|work| work.physical.as_mut())
        else {
            return;
        };
        if physical.incarnation != incarnation || physical.retirement != Retirement::InFlight {
            return;
        }
        physical.retirement = if success {
            Retirement::Applied
        } else {
            Retirement::Failed
        };
        if success {
            self.retire(ticket, now);
        }
    }

    // Submitted bindings require a positive applied receipt, even after registry removal.
    pub(crate) fn retire(&mut self, ticket: &Ticket, now: Instant) {
        self.expire_principal(&ticket.principal, now);
        if self
            .work
            .get(ticket)
            .and_then(|work| work.physical.as_ref())
            .is_some_and(|physical| {
                physical.submitted && physical.retirement != Retirement::Applied
            })
        {
            return;
        }
        let Some(work) = self.work.remove(ticket) else {
            return;
        };
        if let Some(physical) = work.physical {
            if self.incarnations.remove(&physical.incarnation).as_ref() != Some(ticket) {
                pulsebeam_runtime::fatal!("ownership incarnation index changed its ticket");
            }
            self.retirements.remove(ticket);
        }
        if work.pending {
            release_credit(&mut self.in_flight, 1);
            release_credit(&mut self.result_bytes, MAX_RESULT_BYTES);
        }
        if let Some(record) = self.records.get_mut(&ticket.principal) {
            release_credit(&mut record.pins, 1);
            if let Some(family) = record.family.as_mut() {
                if family.proof == ticket.proof && family.sequence == ticket.sequence {
                    family.pending = false;
                    if let Some(result) = family.result.take() {
                        release_credit(&mut self.result_bytes, result.bytes.len());
                    }
                }
                if record.pins == 0 {
                    family.idle = Some(now.checked_add(IDLE_LIFETIME).unwrap_or(now));
                }
            }
        }
        self.reschedule(&ticket.principal);
    }

    pub(crate) fn expire(&mut self, now: Instant, budget: usize) -> usize {
        let mut spent = 0;
        while spent < budget {
            let Some((deadline, principal)) = self.deadlines.first() else {
                break;
            };
            if *deadline > now {
                break;
            }
            let principal = principal.clone();
            self.expire_principal(&principal, now);
            add_credit(&mut spent, 1);
        }
        spent
    }

    fn family(&self, ticket: &Ticket) -> Result<&Family, Error> {
        let family = self
            .records
            .get(&ticket.principal)
            .and_then(|record| record.family.as_ref())
            .ok_or(Error::UnknownProof)?;
        if family.proof != ticket.proof || family.sequence != ticket.sequence || family.expired {
            return Err(Error::Replaced);
        }
        Ok(family)
    }

    fn expire_principal(&mut self, principal: &PrincipalKey, now: Instant) {
        let Some(mut record) = self.take_record(principal) else {
            return;
        };
        let expired = record
            .fresh
            .iter()
            .filter(|fresh| fresh.deadline <= now)
            .count();
        record.fresh.retain(|fresh| fresh.deadline > now);
        release_credit(&mut self.fresh, expired);
        if let Some(family) = record.family.as_mut() {
            if family
                .result
                .as_ref()
                .is_some_and(|result| result.deadline <= now)
                && let Some(result) = family.result.take()
            {
                release_credit(&mut self.result_bytes, result.bytes.len());
            }
            if record.pins == 0 && family.idle.is_some_and(|deadline| deadline <= now) {
                family.expired = true;
                family.idle = None;
            }
        }
        if record.pins == 0
            && record.fresh.is_empty()
            && record.family.as_ref().is_none_or(|family| family.expired)
        {
            return;
        }
        self.put_record(principal.clone(), record);
    }

    fn take_record(&mut self, principal: &PrincipalKey) -> Option<Record> {
        let record = self.records.remove(principal)?;
        if let Some(deadline) = record.deadline {
            self.deadlines.remove(&(deadline, principal.clone()));
        }
        Some(record)
    }

    fn put_record(&mut self, principal: PrincipalKey, mut record: Record) {
        record.deadline = record
            .fresh
            .iter()
            .map(|fresh| fresh.deadline)
            .chain(record.family.iter().filter_map(|family| family.idle))
            .chain(
                record
                    .family
                    .iter()
                    .filter_map(|family| family.result.as_ref().map(|result| result.deadline)),
            )
            .min();
        if let Some(deadline) = record.deadline {
            self.deadlines.insert((deadline, principal.clone()));
        }
        self.records.insert(principal, record);
    }

    fn reschedule(&mut self, principal: &PrincipalKey) {
        if let Some(record) = self.take_record(principal) {
            self.put_record(principal.clone(), record);
        }
    }
}

// Broken accounting must stop admission, not silently free authority budget.
fn add_credit(counter: &mut usize, amount: usize) {
    *counter = counter
        .checked_add(amount)
        .unwrap_or_else(|| pulsebeam_runtime::fatal!("ownership accounting overflow"));
}

fn release_credit(counter: &mut usize, amount: usize) {
    *counter = counter
        .checked_sub(amount)
        .unwrap_or_else(|| pulsebeam_runtime::fatal!("ownership accounting released twice"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};

    fn principal(name: &str) -> PrincipalKey {
        PrincipalKey {
            project: ProjectId::new(),
            participant: ParticipantId::new(name).unwrap(),
            roomless_attempt: None,
        }
    }

    fn sequence(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    fn start(
        authority: &mut Authority,
        principal: &PrincipalKey,
        proof: Proof,
        value: u64,
        now: Instant,
    ) -> Ticket {
        let Admission::Start(ticket) = authority
            .begin(
                principal,
                proof,
                sequence(value),
                RequestBinding([u8::try_from(value).unwrap(); 32]),
                now,
            )
            .unwrap()
        else {
            panic!("expected a new fenced attempt");
        };
        ticket
    }

    fn check(authority: &Authority) {
        assert!(authority.records.len() <= authority.limits.records);
        assert!(authority.deadlines.len() <= authority.records.len());
        assert!(authority.in_flight <= authority.limits.in_flight);
        assert_eq!(
            authority.in_flight,
            authority.work.values().filter(|work| work.pending).count()
        );
        assert_eq!(
            authority.fresh,
            authority
                .records
                .values()
                .map(|record| record.fresh.len())
                .sum::<usize>()
        );
        assert_eq!(
            authority.work.len(),
            authority
                .records
                .values()
                .map(|record| record.pins)
                .sum::<usize>()
        );
        let cached = authority
            .records
            .values()
            .filter_map(|record| record.family.as_ref())
            .filter_map(|family| family.result.as_ref())
            .map(|result| result.bytes.len())
            .sum::<usize>();
        assert_eq!(
            authority.result_bytes,
            cached + authority.in_flight * MAX_RESULT_BYTES
        );
        assert!(authority.result_bytes <= authority.limits.result_bytes);
        assert_eq!(
            authority.incarnations.len(),
            authority
                .work
                .values()
                .filter(|work| work.physical.is_some())
                .count()
        );
        assert_eq!(
            authority.retirements.len(),
            authority
                .work
                .values()
                .filter(|work| work
                    .physical
                    .as_ref()
                    .is_some_and(|physical| physical.retirement == Retirement::Queued))
                .count()
        );
        for (incarnation, ticket) in &authority.incarnations {
            assert!(
                authority
                    .work
                    .get(ticket)
                    .and_then(|work| work.physical.as_ref())
                    .is_some_and(|physical| physical.incarnation == *incarnation)
            );
        }
    }

    #[test]
    fn conditional_claim_replay_and_quiescence_survive_lost_replacement_notice() {
        let mut authority = Authority::default();
        let mut entropy = StdRng::seed_from_u64(1);
        let principal = principal("alice");
        let now = Instant::now();
        let a = authority
            .issue(principal.clone(), now, &mut entropy)
            .unwrap();
        let rival = authority
            .issue(principal.clone(), now, &mut entropy)
            .unwrap();
        assert_ne!(a, rival);
        let old = start(&mut authority, &principal, a, 1, now);
        assert_eq!(
            authority.begin(&principal, rival, sequence(1), RequestBinding([1; 32]), now),
            Err(Error::Replaced)
        );
        for _ in 1..MAX_WAITERS {
            assert_eq!(
                authority.begin(&principal, a, sequence(1), RequestBinding([1; 32]), now),
                Ok(Admission::Pending(old.clone()))
            );
        }
        assert_eq!(
            authority.begin(&principal, a, sequence(1), RequestBinding([1; 32]), now),
            Err(Error::Capacity)
        );
        authority.detach_waiter(&old);
        assert_eq!(
            authority.begin(&principal, a, sequence(1), RequestBinding([1; 32]), now),
            Ok(Admission::Pending(old.clone()))
        );
        assert_eq!(
            authority.begin(&principal, a, sequence(1), RequestBinding([9; 32]), now),
            Err(Error::ChangedAttempt)
        );
        authority
            .commit(&old, b"first answer".to_vec(), now)
            .unwrap();
        assert_eq!(
            authority.result(&old, now).unwrap().as_ref(),
            b"first answer"
        );
        assert_eq!(
            authority.begin(&principal, a, sequence(1), RequestBinding([1; 32]), now),
            Ok(Admission::Replay(old.clone()))
        );
        let b = authority
            .issue(principal.clone(), now, &mut entropy)
            .unwrap();
        assert!(
            authority.is_current(&old),
            "issuing a capability cannot displace"
        );
        let new = start(&mut authority, &principal, b, 1, now);
        assert!(!authority.is_current(&old));
        assert_eq!(
            authority.commit(&new, vec![], now),
            Err(Error::QuiescencePending)
        );
        authority.retire(&old, now);
        authority.commit(&new, b"new answer".to_vec(), now).unwrap();
        assert!(authority.is_current(&new));
        assert_eq!(
            authority.begin(&principal, a, sequence(2), RequestBinding([2; 32]), now),
            Err(Error::Replaced)
        );
        authority.retire(&old, now);
        assert!(
            authority.is_current(&new),
            "stale cleanup cannot retire the new owner"
        );
        check(&authority);
    }

    #[test]
    fn failed_materialization_retries_exact_request_only_after_retirement() {
        let mut authority = Authority::default();
        let mut entropy = StdRng::seed_from_u64(17);
        let principal = principal("alice");
        let now = Instant::now();
        let proof = authority
            .issue(principal.clone(), now, &mut entropy)
            .unwrap();
        let ticket = start(&mut authority, &principal, proof, 1, now);
        let old = Incarnation {
            participant: crate::entity::fixture_participant_id(),
            connection: ConnectionId::new(),
            transport: NodeTransportAddress::new(
                crate::route::TransportRoute::new(crate::id::ShardId::new(0), 1),
                1,
            ),
        };
        let attach = |authority: &mut Authority, incarnation| {
            let work = authority.work.get_mut(&ticket).unwrap();
            assert!(work.physical.is_none());
            work.physical = Some(Physical {
                incarnation,
                submitted: true,
                retirement: Retirement::Live,
            });
            assert!(
                authority
                    .incarnations
                    .insert(incarnation, ticket.clone())
                    .is_none()
            );
        };
        attach(&mut authority, old);
        authority.retry_failed_materialization(&ticket);
        authority.queue_retirement(old);
        authority.retire(&ticket, now);
        assert_eq!(
            authority.preview(&principal, proof, sequence(1), RequestBinding([1; 32]), now),
            Err(Error::QuiescencePending)
        );
        authority.start_retirement(&ticket);
        authority.finish_retirement(&ticket, old, true, now);
        assert_eq!(
            authority.begin(&principal, proof, sequence(1), RequestBinding([2; 32]), now),
            Err(Error::ChangedAttempt)
        );
        assert_eq!(start(&mut authority, &principal, proof, 1, now), ticket);
        let current = Incarnation {
            connection: ConnectionId::new(),
            ..old
        };
        attach(&mut authority, current);
        check(&authority);
        authority.commit(&ticket, b"answer".to_vec(), now).unwrap();
        assert_eq!(
            authority.begin(&principal, proof, sequence(1), RequestBinding([1; 32]), now),
            Ok(Admission::Replay(ticket.clone()))
        );
        authority.retry_failed_materialization(&ticket);
        authority.queue_retirement(current);
        authority.start_retirement(&ticket);
        authority.finish_retirement(&ticket, old, true, now);
        assert!(
            authority.has_incarnation(current),
            "old cutoff cannot retire the retried incarnation"
        );
        authority.finish_retirement(&ticket, current, true, now);
        assert_eq!(
            authority.begin(&principal, proof, sequence(1), RequestBinding([1; 32]), now),
            Err(Error::ReplayUnavailable)
        );
        check(&authority);
    }

    #[test]
    fn high_water_pins_late_installs_and_survives_result_expiry_and_history_loss() {
        let mut authority = Authority::default();
        let mut entropy = StdRng::seed_from_u64(2);
        let principal = principal("alice");
        let now = Instant::now();
        let proof = authority
            .issue(principal.clone(), now, &mut entropy)
            .unwrap();
        let old = start(&mut authority, &principal, proof, 1, now);
        let current = start(&mut authority, &principal, proof, 2, now);
        assert_eq!(authority.commit(&old, vec![], now), Err(Error::Replaced));
        assert_eq!(
            authority.commit(&current, vec![], now),
            Err(Error::QuiescencePending)
        );
        authority.retire(&old, now);
        assert_eq!(
            authority.commit(&current, vec![0; MAX_RESULT_BYTES + 1], now),
            Err(Error::ResultTooLarge)
        );
        check(&authority);
        authority.commit(&current, b"answer".to_vec(), now).unwrap();
        let later = now + RESULT_LIFETIME;
        assert_eq!(authority.expire(later, 1), 1);
        assert!(authority.is_current(&current));
        assert_eq!(
            authority.begin(
                &principal,
                proof,
                sequence(2),
                RequestBinding([2; 32]),
                later
            ),
            Err(Error::ReplayUnavailable)
        );
        assert_eq!(
            authority.begin(
                &principal,
                proof,
                sequence(1),
                RequestBinding([1; 32]),
                later
            ),
            Err(Error::StaleAttempt)
        );
        authority.retire(&current, later);
        let gone = later + IDLE_LIFETIME;
        assert_eq!(authority.expire(gone, 1), 1);
        assert!(authority.records.is_empty());
        assert_eq!(
            authority.begin(
                &principal,
                proof,
                sequence(3),
                RequestBinding([3; 32]),
                gone
            ),
            Err(Error::UnknownProof)
        );
        let replacement = authority
            .issue(principal.clone(), gone, &mut entropy)
            .unwrap();
        assert_ne!(proof, replacement);
        assert_eq!(
            authority.begin(
                &principal,
                proof,
                sequence(3),
                RequestBinding([3; 32]),
                gone
            ),
            Err(Error::UnknownProof)
        );
        let mut restarted = Authority::default();
        assert_eq!(
            restarted.begin(
                &principal,
                replacement,
                sequence(1),
                RequestBinding([1; 32]),
                gone
            ),
            Err(Error::UnknownProof)
        );
        check(&authority);
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn budgets_expiry_and_principal_binding_do_not_reset_authority() {
        use futures_lite::StreamExt;

        let mut authority = Authority::new(Limits {
            records: 2,
            fresh: 2,
            in_flight: 1,
            result_bytes: MAX_RESULT_BYTES,
        });
        let mut entropy = StdRng::seed_from_u64(3);
        let alice = principal("alice");
        let bob = PrincipalKey {
            project: alice.project,
            participant: ParticipantId::new("bob").unwrap(),
            roomless_attempt: None,
        };
        let now = Instant::now();
        let a = authority.issue(alice.clone(), now, &mut entropy).unwrap();
        assert_eq!(
            authority.begin(&bob, a, sequence(1), RequestBinding([1; 32]), now),
            Err(Error::UnknownProof)
        );
        assert_eq!(authority.records.len(), 1);
        let old = start(&mut authority, &alice, a, 1, now);
        let b = authority.issue(bob.clone(), now, &mut entropy).unwrap();
        assert_eq!(
            authority.begin(&bob, b, sequence(1), RequestBinding([1; 32]), now),
            Err(Error::Capacity)
        );
        assert_eq!(authority.records[&bob].generation, None);
        let mut oversized_allocation = Vec::with_capacity(2 * MAX_RESULT_BYTES);
        oversized_allocation.extend_from_slice(b"answer");
        authority.commit(&old, oversized_allocation, now).unwrap();
        let mut delivery =
            axum::body::Body::from(authority.result(&old, now).unwrap().into_bytes())
                .into_data_stream();
        let frame = delivery.next().await.unwrap().unwrap();
        let last_frame = frame.clone();
        drop(delivery);
        assert_eq!(authority.result_bytes, b"answer".len());
        assert_eq!(
            authority.begin(&bob, b, sequence(1), RequestBinding([1; 32]), now),
            Err(Error::Capacity),
            "cached bytes also count"
        );
        let next = authority.issue(alice.clone(), now, &mut entropy).unwrap();
        assert_eq!(
            authority.issue(alice.clone(), now, &mut entropy),
            Err(Error::Capacity)
        );
        assert_eq!(
            authority.begin(&alice, next, sequence(1), RequestBinding([1; 32]), now),
            Err(Error::Capacity)
        );
        assert!(
            authority.is_current(&old),
            "delivery pressure cannot displace the incumbent"
        );
        drop(frame);
        assert_eq!(
            authority.begin(&alice, next, sequence(1), RequestBinding([1; 32]), now),
            Err(Error::Capacity)
        );
        drop(last_frame);
        let new = start(&mut authority, &alice, next, 1, now);
        assert_eq!(
            authority.commit(&new, vec![], now),
            Err(Error::QuiescencePending)
        );
        authority.retire(&old, now);
        authority
            .commit(&new, b"final answer".to_vec(), now)
            .unwrap();
        let mut delivery =
            axum::body::Body::from(authority.result(&new, now).unwrap().into_bytes())
                .into_data_stream();
        let held_frame = delivery.next().await.unwrap().unwrap();
        drop(delivery);
        let later = now + FRESH_LIFETIME;
        assert_eq!(authority.expire(later, 0), 0);
        assert_eq!(authority.expire(later, 1), 1);
        assert_eq!(
            authority.begin(&bob, b, sequence(1), RequestBinding([1; 32]), later),
            Err(Error::UnknownProof)
        );
        assert_eq!(authority.deadlines.len(), 0);
        assert!(authority.is_current(&new));
        let fresh_b = authority.issue(bob.clone(), later, &mut entropy).unwrap();
        assert_eq!(
            authority.begin(&bob, fresh_b, sequence(1), RequestBinding([1; 32]), later),
            Err(Error::Capacity),
            "cache expiry cannot reclaim a delivered frame's allocation"
        );
        assert_eq!(authority.records[&bob].generation, None);
        drop(held_frame);
        let other = start(&mut authority, &bob, fresh_b, 1, later);
        authority.retire(&other, later);
        check(&authority);
    }

    #[test]
    fn proof_wire_format_and_debug_do_not_disclose_secrets() {
        let mut authority = Authority::default();
        let mut entropy = StdRng::seed_from_u64(4);
        let principal = principal("private-external-name");
        let now = Instant::now();
        for room in ["main", "breakout"] {
            let room_id = crate::entity::RoomId::new(room).unwrap();
            let authorization = VerifiedAuthorization {
                token_id: "00000000-0000-4000-8000-000000000001".parse().unwrap(),
                grants: pulsebeam_core::auth::grants::GrantSet::default(),
                project_id: principal.project,
                participant_id: principal.participant,
                room_id: Some(room_id),
                expiry: pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(10_000),
            };
            assert_eq!(PrincipalKey::from_verified(&authorization), principal);
        }
        let proof = authority
            .issue(principal.clone(), now, &mut entropy)
            .unwrap();
        let encoded = proof.encode();
        assert_eq!(encoded.len(), 52);
        assert_eq!(Proof::parse(&encoded), Ok(proof));
        for invalid in [
            String::new(),
            "A".repeat(53),
            "!".repeat(52),
            encoded.to_lowercase(),
        ] {
            assert_eq!(Proof::parse(&invalid), Err(Error::InvalidProof));
        }
        let diagnostic = format!("{principal:?} {proof:?} {:?}", RequestBinding([42; 32]));
        assert!(!diagnostic.contains(&encoded));
        assert!(!diagnostic.contains("private-external-name"));
        assert!(!diagnostic.contains("42"));
        let main = RoomId::new("main").unwrap();
        let binding =
            RequestBinding::for_request(SignalingProtocol::State, &main, "offer", "secret")
                .unwrap();
        assert_eq!(
            RequestBinding::for_request(SignalingProtocol::State, &main, "offer", "secret"),
            Ok(binding)
        );
        for (protocol, room, offer, credential) in [
            (SignalingProtocol::Whip, "main", "offer", "secret"),
            (SignalingProtocol::Whep, "main", "offer", "secret"),
            (SignalingProtocol::State, "breakout", "offer", "secret"),
            (SignalingProtocol::State, "main", "other", "secret"),
            (SignalingProtocol::State, "main", "offer", "new-secret"),
        ] {
            assert_ne!(
                RequestBinding::for_request(
                    protocol,
                    &RoomId::new(room).unwrap(),
                    offer,
                    credential
                ),
                Ok(binding)
            );
        }
        assert_ne!(
            RequestBinding::for_request(
                SignalingProtocol::State,
                &RoomId::new("ab").unwrap(),
                "c",
                "secret"
            ),
            RequestBinding::for_request(
                SignalingProtocol::State,
                &RoomId::new("a").unwrap(),
                "bc",
                "secret"
            ),
        );
        for _ in 1..FRESH_PER_PRINCIPAL {
            authority
                .issue(principal.clone(), now, &mut entropy)
                .unwrap();
        }
        assert_eq!(
            authority.issue(principal.clone(), now, &mut entropy),
            Err(Error::Capacity)
        );
        check(&authority);
        let mut full = Authority::new(Limits {
            records: 1,
            ..Limits::default()
        });
        full.issue(principal, now, &mut entropy).unwrap();
        assert_eq!(
            full.issue(self::principal("other"), now, &mut entropy),
            Err(Error::Capacity)
        );
        check(&full);
    }
}
