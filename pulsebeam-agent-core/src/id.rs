use alloc::string::String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Generation(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DataChannelId(u16);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
