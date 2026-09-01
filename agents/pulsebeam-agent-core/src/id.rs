#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Generation(u64);

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct OperationId(u64);

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct TimerId(u64);

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct ChannelId(u64);

impl ChannelId {
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Generation {
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl OperationId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl TimerId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

pub(crate) struct IdGenerator {
    next: u64,
}

impl IdGenerator {
    pub(crate) const fn new() -> Self {
        Self { next: 1 }
    }

    fn next(&mut self) -> u64 {
        let id = self.next;
        debug_assert_ne!(id, u64::MAX, "agent correlation ID space exhausted");
        self.next = self.next.wrapping_add(1).max(1);
        id
    }

    pub(crate) fn generation(&mut self) -> Generation {
        Generation(self.next())
    }

    pub(crate) fn operation(&mut self) -> OperationId {
        OperationId(self.next())
    }

    pub(crate) fn timer(&mut self) -> TimerId {
        TimerId(self.next())
    }
}
