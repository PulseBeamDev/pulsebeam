//! One bounded raw protobuf batch per DataChannel message. HTTP selects the profile.

use alloc::vec::Vec;
use prost::Message;

use super::{
    ClientStateBatch, ServerStateBatch,
    validation::{self, ValidationError},
};

mod preflight;

pub const MAX_FRAME_SIZE: usize = 4096;
pub const MAX_RECORDS: usize = 16;

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
