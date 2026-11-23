//! PRODUCTION WIRE CONTRACT — EDIT WITH EXTREME CARE. Changes can break deployed clients, mixed-version nodes, and production traffic. Require compatibility review and wire-vector tests.
//!
//! Reliable control for [FabricPacket #275 §5.4](https://github.com/PulseBeamDev/pulsebeam/issues/275).
//! `proto/data_control.proto` assigns fields. One control message contains one
//! directional protobuf record, at most 128 bytes. Use ascending field numbers,
//! minimal varints and omit implicit defaults. `fixed64` fields use protobuf's
//! little-endian encoding. Present optional zero values are not implicit defaults.
//! Unknown/duplicate fields and alternate encodings are not v1 records; negotiate
//! the version and role before dispatch. These bytes never carry DATA fragments.
//! The literal byte vectors are in `data_control/tests.rs`.

use alloc::vec::Vec;
use core::num::NonZeroU64;
use prost::Message;

include!(concat!(env!("OUT_DIR"), "/pulsebeam.data.v1.rs"));

pub const MAX_CONTROL_FRAME_BYTES: usize = 128;
pub const VERSION: u8 = 1;

/// Match against the installed publisher binding, not an identity supplied by the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublisherBinding {
    pub source: NonZeroU64,
}

/// Match against the installed subscriber binding on this association and direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriberBinding {
    pub source: NonZeroU64,
    pub branch: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    FrameSize,
    Encoding,
    Structure,
    Binding,
}

pub fn encode<R: Record>(binding: R::Binding, record: &R) -> Result<Vec<u8>, Error> {
    record.check(binding)?;
    let size = record.encoded_len();
    if !(1..=MAX_CONTROL_FRAME_BYTES).contains(&size) {
        return Err(Error::FrameSize);
    }
    Ok(record.encode_to_vec())
}

pub fn decode<R: Record>(binding: R::Binding, bytes: &[u8]) -> Result<R, Error> {
    if !(1..=MAX_CONTROL_FRAME_BYTES).contains(&bytes.len()) {
        return Err(Error::FrameSize);
    }
    let record = R::decode(bytes).map_err(|_| Error::Encoding)?;
    if record.encode_to_vec() != bytes {
        return Err(Error::Encoding);
    }
    record.check(binding)?;
    Ok(record)
}

mod sealed {
    pub trait Sealed {}
}

/// A directional record. The caller selects the type and binding from the
/// authenticated channel, never by trying candidate decoders against bytes.
#[allow(
    private_bounds,
    reason = "only the generated directional records implement Record"
)]
pub trait Record: Message + Default + sealed::Sealed {
    type Binding;
    fn check(&self, binding: Self::Binding) -> Result<(), Error>;
}

fn source(unit: Option<&SourceUnit>, expected: NonZeroU64) -> Result<(), Error> {
    let unit = unit.ok_or(Error::Structure)?;
    if unit.unit_seq.is_none() {
        return Err(Error::Structure);
    }
    if unit.source_incarnation != expected.get() {
        return Err(Error::Binding);
    }
    Ok(())
}

fn branch_id(
    unit: Option<&BranchUnit>,
    source_incarnation: NonZeroU64,
) -> Result<NonZeroU64, Error> {
    let unit = unit.ok_or(Error::Structure)?;
    source(unit.source.as_ref(), source_incarnation)?;
    NonZeroU64::new(unit.branch_incarnation).ok_or(Error::Structure)
}

fn expected_branch(value: &Expectation, source: NonZeroU64) -> Result<NonZeroU64, Error> {
    if !(1..=64).contains(&value.fragment_count) {
        return Err(Error::Structure);
    }
    branch_id(value.unit.as_ref(), source)
}

fn require_branch(actual: NonZeroU64, bound: NonZeroU64) -> Result<(), Error> {
    (actual == bound).then_some(()).ok_or(Error::Binding)
}

fn expectation(value: &Expectation, binding: SubscriberBinding) -> Result<(), Error> {
    require_branch(expected_branch(value, binding.source)?, binding.branch)
}

fn request(index: Option<u32>, id: u64) -> Result<(), Error> {
    if !index.is_some_and(|index| index < 64) || id == 0 {
        return Err(Error::Structure);
    }
    Ok(())
}

fn original(value: &OriginalDemand, binding: PublisherBinding) -> Result<(), Error> {
    request(value.fragment_index, value.request_id)?;
    source(value.unit.as_ref(), binding.source)
}

fn repair(value: &RepairDemand, binding: SubscriberBinding) -> Result<(), Error> {
    request(value.fragment_index, value.request_id)?;
    require_branch(
        branch_id(value.unit.as_ref(), binding.source)?,
        binding.branch,
    )
}

fn outcome(value: i32) -> Result<(), Error> {
    match ControlOutcome::try_from(value) {
        Ok(ControlOutcome::Confirmed | ControlOutcome::Failed | ControlOutcome::Unknown) => Ok(()),
        _ => Err(Error::Structure),
    }
}

impl sealed::Sealed for PublisherControl {}
impl Record for PublisherControl {
    type Binding = PublisherBinding;

    fn check(&self, binding: Self::Binding) -> Result<(), Error> {
        let publisher_control::Record::Unavailable(value) =
            self.record.as_ref().ok_or(Error::Structure)?;
        original(value.demand.as_ref().ok_or(Error::Structure)?, binding)
    }
}

impl sealed::Sealed for PublisherObservation {}
impl Record for PublisherObservation {
    type Binding = PublisherBinding;

    fn check(&self, binding: Self::Binding) -> Result<(), Error> {
        use publisher_observation::Record;
        match self.record.as_ref().ok_or(Error::Structure)? {
            Record::Admission(unit) => source(Some(unit), binding.source),
            Record::RequestOriginal(value) => original(value, binding),
            Record::Delivery(value) => {
                let expected = value.expectation.as_ref().ok_or(Error::Structure)?;
                expected_branch(expected, binding.source)?;
                outcome(value.outcome)
            }
        }
    }
}

impl sealed::Sealed for SubscriberControl {}
impl Record for SubscriberControl {
    type Binding = SubscriberBinding;

    fn check(&self, binding: Self::Binding) -> Result<(), Error> {
        use subscriber_control::Record;
        match self.record.as_ref().ok_or(Error::Structure)? {
            Record::ExpectationOutcome(value) => {
                expectation(value.expectation.as_ref().ok_or(Error::Structure)?, binding)?;
                outcome(value.outcome)
            }
            Record::AssemblyReceipt(value) => expectation(value, binding),
            Record::RequestRepair(value) | Record::CancelRepair(value) => repair(value, binding),
        }
    }
}

impl sealed::Sealed for SubscriberObservation {}
impl Record for SubscriberObservation {
    type Binding = SubscriberBinding;

    fn check(&self, binding: Self::Binding) -> Result<(), Error> {
        use subscriber_observation::Record;
        match self.record.as_ref().ok_or(Error::Structure)? {
            Record::Expectation(value) | Record::ReceiptAck(value) => expectation(value, binding),
            Record::Repair(value) => {
                if !RepairStatus::try_from(value.status)
                    .is_ok_and(|status| status != RepairStatus::Unspecified)
                {
                    return Err(Error::Structure);
                }
                repair(value.demand.as_ref().ok_or(Error::Structure)?, binding)
            }
        }
    }
}

#[cfg(test)]
mod tests;
