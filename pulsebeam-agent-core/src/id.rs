use alloc::string::String;

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(u64);

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequestId(u64);

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct OperationId(u64);

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimerId(u64);

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct DataChannelId(u16);

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct DataChannelLabel(String);

pub(crate) struct IdGenerator {
    next: u64,
}

impl IdGenerator {
    pub(crate) const fn new() -> Self {
        Self { next: 1 }
    }

    fn next(&mut self) -> u64 {
        let id = self.next;

        self.next = self.next.checked_add(1).expect("ID space exhausted");

        id
    }

    pub(crate) fn request(&mut self) -> RequestId {
        RequestId(self.next())
    }

    pub(crate) fn timer(&mut self) -> TimerId {
        TimerId(self.next())
    }

    pub(crate) fn generation(&mut self) -> Generation {
        Generation(self.next())
    }
}
