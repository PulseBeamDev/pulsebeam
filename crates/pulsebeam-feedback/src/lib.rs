//! Caller-clocked transport feedback accounting. Wire counters must be unwrapped
//! before entry. Times are monotonic durations from a stable instance origin;
//! receiver-relative times are not synchronized with the sender clock.

#[cfg(test)]
mod allocation;
mod receiver;
mod sender;
pub use receiver::{Observation, Receipt, Receiver, ReceiverConfig, ReportOrder, ReportWindow};
pub use sender::{
    AvailabilityChange, FeedbackMode, FeedbackTiming, HistoryCounters, IndependentReport,
    LossOrigin, PreparedProfile, SendCommit, SendFeedback, SendIdentity, SendReport, SendStatus,
    Sender, SenderConfig, SenderError, SenderInputs, SentObservation, SentPacketId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SpaceId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PacketId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Epoch(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ecn {
    NotEct,
    Ect1,
    Ect0,
    Ce,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiverTime {
    Micros(i64),
    OverRange,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidConfig,
    InvalidRange,
    WrongEpoch,
    WrongSpace,
    StaleReport,
    Exhausted,
}
