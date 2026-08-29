#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Generation(u64);

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct RequestId(u64);

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct OperationId(u64);

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct TimerId(u64);

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct DataChannelId(u64);

impl Generation {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl RequestId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl TimerId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl DataChannelId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[allow(
    dead_code,
    reason = "the session state machine allocates these identifiers in the next plan"
)]
pub(crate) struct IdGenerator {
    next: u64,
}

#[allow(
    dead_code,
    reason = "the session state machine allocates these identifiers in the next plan"
)]
impl IdGenerator {
    pub(crate) const fn new() -> Self {
        Self { next: 1 }
    }

    fn next(&mut self) -> Option<u64> {
        let id = self.next;
        self.next = self.next.checked_add(1)?;
        Some(id)
    }

    pub(crate) fn request(&mut self) -> Option<RequestId> {
        Some(RequestId(self.next()?))
    }

    pub(crate) fn timer(&mut self) -> Option<TimerId> {
        Some(TimerId(self.next()?))
    }

    pub(crate) fn data_channel(&mut self) -> Option<DataChannelId> {
        Some(DataChannelId(self.next()?))
    }

    pub(crate) fn generation(&mut self) -> Option<Generation> {
        Some(Generation(self.next()?))
    }
}
