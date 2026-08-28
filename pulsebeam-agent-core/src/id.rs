use alloc::string::String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Generation(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DataChannelId(u16);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DataChannelLabel(String);
