//! One bounded protobuf message per DataChannel message. HTTP selects the profile.

use alloc::vec::Vec;
use prost::Message;

use super::{
    ClientStateBatch, NativeDataControl, ServerStateBatch, native_data_control,
    validation::{self, ValidationError},
};

#[cfg(test)]
mod catalog_tests;
mod preflight;

pub const MAX_FRAME_SIZE: usize = 4096;
pub const MAX_RECORDS: usize = 16;
const MAX_DATA_CONTROL: usize = crate::data_control::MAX_CONTROL_FRAME_BYTES;
const MAX_DATA_CONTROL_FRAME: usize = MAX_DATA_CONTROL + 17;
pub(super) const DATA_PUBLISHER_FIELD: u32 = 4;
pub(super) const DATA_SUBSCRIBER_FIELD: u32 = 5;

#[derive(Debug, PartialEq)]
pub enum NativeFrame<T> {
    State(T),
    Data(NativeDataControl),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeError {
    Validation(ValidationError),
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    FrameSize,
    InvalidProtobuf,
    Structure,
    Validation(ValidationError),
}

pub fn encode_client(batch: &ClientStateBatch) -> Result<Vec<u8>, EncodeError> {
    validation::validate_client_batch(batch).map_err(EncodeError::Validation)?;
    encode(batch)
}

pub fn encode_server(batch: &ServerStateBatch) -> Result<Vec<u8>, EncodeError> {
    validation::validate_server_batch(batch).map_err(EncodeError::Validation)?;
    encode(batch)
}

pub fn decode_client(frame: &[u8]) -> Result<ClientStateBatch, DecodeError> {
    let batch = decode(frame, preflight::check_client)?;
    validation::validate_client_batch(&batch).map_err(DecodeError::Validation)?;
    Ok(batch)
}

pub fn decode_server(frame: &[u8]) -> Result<ServerStateBatch, DecodeError> {
    let batch = decode(frame, preflight::check_server)?;
    validation::validate_server_batch(&batch).map_err(DecodeError::Validation)?;
    Ok(batch)
}

/// Only negotiated Reliable DATA owners may use this carrier. The inner
/// record is encoded separately with its installed directional binding.
pub fn encode_data_control(control: &NativeDataControl) -> Result<Vec<u8>, EncodeError> {
    if !valid_data_control(control) {
        return Err(EncodeError::Validation(ValidationError::Batch));
    }
    encode(control)
}

pub fn decode_client_frame(frame: &[u8]) -> Result<NativeFrame<ClientStateBatch>, DecodeError> {
    decode_frame(frame, decode_client)
}

pub fn decode_server_frame(frame: &[u8]) -> Result<NativeFrame<ServerStateBatch>, DecodeError> {
    decode_frame(frame, decode_server)
}

fn decode_frame<T>(
    frame: &[u8],
    state: impl FnOnce(&[u8]) -> Result<T, DecodeError>,
) -> Result<NativeFrame<T>, DecodeError> {
    match decode_data_control_frame(frame) {
        Some(control) => control.map(NativeFrame::Data),
        None => state(frame).map(NativeFrame::State),
    }
}

/// Route an explicitly tagged frame at an already authenticated stream owner.
/// A malformed control frame never falls through to the State decoder.
pub fn decode_data_control_frame(frame: &[u8]) -> Option<Result<NativeDataControl, DecodeError>> {
    // Standalone control has one explicit length-delimited protobuf field.
    // State preflight rejects these reserved fields anywhere in a State batch.
    const PUBLISHER_TAG: u8 = 0x22;
    const SUBSCRIBER_TAG: u8 = 0x2a;
    if !matches!(frame.first().copied(), Some(PUBLISHER_TAG | SUBSCRIBER_TAG)) {
        return None;
    }
    Some(decode_data_control(frame))
}

fn decode_data_control(frame: &[u8]) -> Result<NativeDataControl, DecodeError> {
    if frame.len() > MAX_DATA_CONTROL_FRAME {
        return Err(DecodeError::FrameSize);
    }
    let control = NativeDataControl::decode(frame).map_err(|_| DecodeError::InvalidProtobuf)?;
    if !valid_data_control(&control) || control.encode_to_vec() != frame {
        return Err(DecodeError::Structure);
    }
    Ok(control)
}

fn valid_data_control(control: &NativeDataControl) -> bool {
    use native_data_control::Endpoint;
    match control.endpoint.as_ref() {
        Some(Endpoint::Publisher(control) | Endpoint::Subscriber(control)) => {
            control.owned != 0 && (1..=MAX_DATA_CONTROL).contains(&control.record.len())
        }
        None => false,
    }
}

fn encode(batch: &impl Message) -> Result<Vec<u8>, EncodeError> {
    let len = batch.encoded_len();
    if len == 0 || len > MAX_FRAME_SIZE {
        return Err(EncodeError::TooLarge);
    }
    Ok(batch.encode_to_vec())
}

fn decode<M: Message + Default>(
    frame: &[u8],
    preflight: fn(&[u8]) -> Result<(), DecodeError>,
) -> Result<M, DecodeError> {
    if frame.is_empty() || frame.len() > MAX_FRAME_SIZE {
        return Err(DecodeError::FrameSize);
    }
    preflight(frame)?;
    M::decode(frame).map_err(|_| DecodeError::InvalidProtobuf)
}

#[cfg(test)]
mod tests;
