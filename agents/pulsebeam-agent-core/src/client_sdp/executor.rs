use alloc::{collections::VecDeque, vec::Vec};

use super::{
    Allocation, AllocationMode, ApplyOutcome, Candidate, CandidateId, Context, Error, Preparation,
};
use crate::OperationId;

const MAX_PENDING: usize = 32;
const MAX_RETAINED_BYTES: usize = 2 * super::MAX_SDP_BYTES;

enum Request {
    Prepare(Vec<Preparation>),
    Allocate(Vec<Allocation>, AllocationMode),
    Retire(Vec<String>),
}

struct Work {
    operation: OperationId,
    request: Request,
    bytes: usize,
}

impl Request {
    fn retained_bytes(&self) -> Result<usize, Error> {
        let backing = match self {
            Self::Prepare(requests) => requests.capacity().saturating_mul(size_of::<Preparation>()),
            Self::Allocate(allocations, _) => allocations
                .capacity()
                .saturating_mul(size_of::<Allocation>()),
            Self::Retire(mids) => mids.capacity().saturating_mul(size_of::<String>()),
        };
        let mut bytes = size_of::<Work>().saturating_add(backing);
        if bytes > MAX_RETAINED_BYTES {
            return Err(Error::Capacity);
        }
        match self {
            Self::Prepare(requests) => {
                for request in requests {
                    bytes = bytes.saturating_add(request.mid.capacity());
                    for owner in [&request.publication, &request.reception]
                        .into_iter()
                        .flatten()
                    {
                        bytes = bytes.saturating_add(owner.key.0.capacity()).saturating_add(
                            owner.group.as_ref().map_or(0, |group| group.0.capacity()),
                        );
                    }
                }
            }
            Self::Allocate(allocations, _) => {
                for allocation in allocations {
                    if let crate::MediaSlot::LocalAudio(name) | crate::MediaSlot::LocalVideo(name) =
                        &allocation.slot
                    {
                        bytes = bytes.saturating_add(name.capacity());
                    }
                }
            }
            Self::Retire(mids) => {
                for mid in mids {
                    bytes = bytes.saturating_add(mid.capacity());
                }
            }
        }
        if bytes > MAX_RETAINED_BYTES {
            return Err(Error::Capacity);
        }
        Ok(bytes)
    }

    fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Prepare(_) => {}
            Self::Retire(mids) => {
                if mids.is_empty()
                    || mids.iter().any(|mid| !super::sdp::token(mid))
                    || mids.iter().collect::<super::BTreeSet<_>>().len() != mids.len()
                {
                    return Err(Error::Identifier);
                }
            }
            Self::Allocate(allocations, _) => {
                if allocations.is_empty()
                    || allocations
                        .iter()
                        .map(|allocation| &allocation.slot)
                        .collect::<super::BTreeSet<_>>()
                        .len()
                        != allocations.len()
                {
                    return Err(Error::Identifier);
                }
                for allocation in allocations {
                    allocation.validate()?;
                }
            }
        }
        Ok(())
    }
}

struct Running {
    work: Work,
    candidate: Option<CandidateId>,
    cancelled: bool,
}

impl Running {
    fn should_abort(&self) -> bool {
        // Allocation and retirement change physical media. Drain their actual
        // application so cancellation cannot forget irreversible history.
        self.cancelled && matches!(&self.work.request, Request::Prepare(_))
    }
}

/// Per-peer execution bookkeeping, not resource or signaling policy.
/// Hosts supply actual descriptions and report actual platform application.
pub struct Executor {
    context: Context,
    pending: VecDeque<Work>,
    running: Option<Running>,
    latest: Option<OperationId>,
    retained_bytes: usize,
}

impl Executor {
    pub fn new(context: Context) -> Self {
        Self {
            context,
            pending: VecDeque::new(),
            running: None,
            latest: None,
            retained_bytes: 0,
        }
    }

    pub fn context_mut(&mut self) -> &mut Context {
        &mut self.context
    }

    pub fn enqueue(
        &mut self,
        operation: OperationId,
        requests: Vec<Preparation>,
    ) -> Result<(), Error> {
        self.enqueue_work(operation, Request::Prepare(requests))
    }

    pub fn enqueue_retirement(
        &mut self,
        operation: OperationId,
        mids: Vec<String>,
    ) -> Result<(), Error> {
        self.enqueue_work(operation, Request::Retire(mids))
    }

    pub fn enqueue_allocation(
        &mut self,
        operation: OperationId,
        allocations: Vec<Allocation>,
    ) -> Result<(), Error> {
        self.enqueue_allocations(operation, allocations, AllocationMode::Create)
    }

    pub fn enqueue_allocations(
        &mut self,
        operation: OperationId,
        allocations: Vec<Allocation>,
        mode: AllocationMode,
    ) -> Result<(), Error> {
        self.enqueue_work(operation, Request::Allocate(allocations, mode))
    }

    fn enqueue_work(&mut self, operation: OperationId, request: Request) -> Result<(), Error> {
        let bytes = request.retained_bytes();
        if bytes.is_ok() {
            request.validate()?;
        }
        if self.latest.is_some_and(|latest| operation <= latest) {
            return Err(Error::CandidateState);
        }
        self.latest = Some(operation);
        if self
            .pending
            .len()
            .saturating_add(usize::from(self.running.is_some()))
            >= MAX_PENDING
        {
            return Err(Error::Capacity);
        }
        // Keep operation ordering even when backing admission fails.
        let bytes = bytes?;
        if bytes > MAX_RETAINED_BYTES.saturating_sub(self.retained_bytes) {
            return Err(Error::Capacity);
        }
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        self.pending.push_back(Work {
            operation,
            request,
            bytes,
        });
        Ok(())
    }

    pub fn retirement_targets(&self, operation: OperationId) -> Result<&[String], Error> {
        let running = self
            .running
            .as_ref()
            .filter(|running| running.work.operation == operation && !running.cancelled)
            .ok_or(Error::CandidateState)?;
        match &running.work.request {
            Request::Prepare(_) | Request::Allocate(_, _) => Ok(&[]),
            Request::Retire(mids) => {
                self.context.validate_retirements(mids)?;
                Ok(mids)
            }
        }
    }

    pub fn allocation_targets(&self, operation: OperationId) -> Result<&[Allocation], Error> {
        let running = self
            .running
            .as_ref()
            .filter(|running| running.work.operation == operation && !running.cancelled)
            .ok_or(Error::CandidateState)?;
        match &running.work.request {
            Request::Allocate(allocations, mode) => {
                self.context.validate_allocations(allocations, *mode)?;
                Ok(allocations)
            }
            Request::Prepare(_) | Request::Retire(_) => Ok(&[]),
        }
    }

    pub fn allocation_mode(&self, operation: OperationId) -> Result<AllocationMode, Error> {
        let running = self
            .running
            .as_ref()
            .filter(|running| running.work.operation == operation)
            .ok_or(Error::CandidateState)?;
        Ok(match &running.work.request {
            Request::Allocate(_, mode) => *mode,
            _ => AllocationMode::Create,
        })
    }

    pub fn ready(&self) -> bool {
        self.running.is_none() && !self.pending.is_empty()
    }

    pub fn operation(&self) -> Option<OperationId> {
        self.running.as_ref().map(|running| running.work.operation)
    }

    pub fn start(&mut self) -> Option<OperationId> {
        if self.running.is_some() {
            return None;
        }
        let work = self.pending.pop_front()?;
        let operation = work.operation;
        self.running = Some(Running {
            work,
            candidate: None,
            cancelled: false,
        });
        Some(operation)
    }

    /// True means a queued operation was removed and can complete immediately.
    /// An in-flight operation instead waits for its real platform outcome.
    pub fn cancel(&mut self, operation: OperationId) -> bool {
        if let Some(running) = &mut self.running
            && running.work.operation == operation
        {
            running.cancelled = true;
            return false;
        }
        if let Some(index) = self
            .pending
            .iter()
            .position(|work| work.operation == operation)
            && let Some(work) = self.pending.remove(index)
        {
            self.retained_bytes = self.retained_bytes.saturating_sub(work.bytes);
            return true;
        }
        false
    }

    pub fn cancelled(&self, operation: OperationId) -> bool {
        self.running
            .as_ref()
            .is_some_and(|running| running.work.operation == operation && running.cancelled)
    }

    pub fn should_abort(&self, operation: OperationId) -> bool {
        self.running
            .as_ref()
            .filter(|running| running.work.operation == operation)
            .is_some_and(Running::should_abort)
    }

    pub fn prepare(
        &mut self,
        operation: OperationId,
        actual_offer: &str,
    ) -> Result<Candidate, Error> {
        let running = self
            .running
            .as_mut()
            .filter(|running| running.work.operation == operation)
            .ok_or(Error::CandidateState)?;
        if running.should_abort() || running.candidate.is_some() {
            return Err(Error::CandidateState);
        }
        let candidate = match &running.work.request {
            Request::Prepare(requests) => self.context.prepare(actual_offer, requests)?,
            Request::Allocate(_, _) => return Err(Error::CandidateState),
            Request::Retire(mids) => self.context.prepare_retirement(actual_offer, mids)?,
        };
        running.candidate = Some(candidate.id());
        Ok(candidate)
    }

    pub fn prepare_allocated(
        &mut self,
        operation: OperationId,
        actual_offer: &str,
        slots: &[crate::SlotBinding],
    ) -> Result<Candidate, Error> {
        let running = self
            .running
            .as_mut()
            .filter(|running| running.work.operation == operation && running.candidate.is_none())
            .ok_or(Error::CandidateState)?;
        let Request::Allocate(allocations, mode) = &running.work.request else {
            return Err(Error::CandidateState);
        };
        let candidate = self
            .context
            .prepare_allocation(actual_offer, allocations, slots, *mode)?;
        running.candidate = Some(candidate.id());
        Ok(candidate)
    }

    pub fn prepare_abandoned_allocation(
        &mut self,
        operation: OperationId,
        actual_offer: &str,
        stopped_mids: &[String],
    ) -> Result<Candidate, Error> {
        let running = self
            .running
            .as_mut()
            .filter(|running| running.work.operation == operation)
            .ok_or(Error::CandidateState)?;
        if !matches!(&running.work.request, Request::Allocate(_, _)) {
            return Err(Error::CandidateState);
        }
        if let Some(candidate) = running.candidate.take() {
            self.context.finish(candidate, ApplyOutcome::NotApplied)?;
        }
        running.cancelled = true;
        let candidate = self
            .context
            .prepare_abandoned_allocation(actual_offer, stopped_mids)?;
        running.candidate = Some(candidate.id());
        Ok(candidate)
    }

    pub fn finish(&mut self, operation: OperationId, applied: bool) -> Result<ApplyOutcome, Error> {
        let running = self
            .running
            .as_ref()
            .filter(|running| running.work.operation == operation)
            .ok_or(Error::CandidateState)?;
        let outcome = match (applied, running.cancelled) {
            (false, _) => ApplyOutcome::NotApplied,
            (true, false) => ApplyOutcome::Applied,
            (true, true) => ApplyOutcome::AppliedButCancelled,
        };
        if let Some(candidate) = running.candidate {
            self.context.finish(candidate, outcome)?;
        } else if applied {
            return Err(Error::CandidateState);
        }
        self.retained_bytes = self.retained_bytes.saturating_sub(running.work.bytes);
        self.running = None;
        Ok(outcome)
    }
}
