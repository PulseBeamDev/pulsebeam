//! Bounded, independent StateBatch frames. Transport ordering is external.

use alloc::{vec, vec::Vec};
use lz4_flex::block::{compress_into, get_maximum_output_size};
use prost::Message;

use super::{
    StateBatch,
    validation::{self, Direction, ValidationError},
};

mod preflight;

pub const MAX_DECODED_SIZE: usize = 4096;
pub const MAX_FRAME_SIZE: usize = 4101;
pub const MAX_RECORDS: usize = 16;
const HEADER_SIZE: usize = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeError {
    Validation(ValidationError),
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    FrameSize,
    Encoding,
    DecodedSize,
    LengthMismatch,
    InvalidBlock,
    InvalidProtobuf,
    Structure,
    Validation(ValidationError),
}

pub fn encode(batch: &StateBatch, direction: Direction) -> Result<Vec<u8>, EncodeError> {
    validation::validate_batch(batch, direction).map_err(EncodeError::Validation)?;
    let len = batch.encoded_len();
    if len == 0 || len > MAX_DECODED_SIZE {
        return Err(EncodeError::TooLarge);
    }
    let protobuf = batch.encode_to_vec();
    let mut compressed = vec![0; get_maximum_output_size(len)];
    let compressed_len =
        compress_into(&protobuf, &mut compressed).map_err(|_| EncodeError::TooLarge)?;
    let (encoding, payload) = if compressed_len < len {
        (
            1,
            compressed
                .get(..compressed_len)
                .ok_or(EncodeError::TooLarge)?,
        )
    } else {
        (0, protobuf.as_slice())
    };
    let mut frame = Vec::with_capacity(HEADER_SIZE.saturating_add(payload.len()));
    frame.push(encoding);
    frame.extend_from_slice(
        &u32::try_from(len)
            .map_err(|_| EncodeError::TooLarge)?
            .to_le_bytes(),
    );
    frame.extend_from_slice(payload);
    Ok(frame)
}

pub fn decode(frame: &[u8], direction: Direction) -> Result<StateBatch, DecodeError> {
    if frame.len() < HEADER_SIZE || frame.len() > MAX_FRAME_SIZE {
        return Err(DecodeError::FrameSize);
    }
    let (&encoding, rest) = frame.split_first().ok_or(DecodeError::FrameSize)?;
    let (length, payload) = rest.split_at_checked(4).ok_or(DecodeError::FrameSize)?;
    let length = u32::from_le_bytes(length.try_into().map_err(|_| DecodeError::FrameSize)?);
    let length = usize::try_from(length).map_err(|_| DecodeError::DecodedSize)?;
    if length == 0 || length > MAX_DECODED_SIZE {
        return Err(DecodeError::DecodedSize);
    }
    let decoded;
    let protobuf = match encoding {
        0 if payload.len() == length => payload,
        0 => return Err(DecodeError::LengthMismatch),
        1 => {
            decoded = decode_lz4(payload, length)?;
            decoded.as_slice()
        }
        _ => return Err(DecodeError::Encoding),
    };
    preflight::check(protobuf)?;
    let batch = StateBatch::decode(protobuf).map_err(|_| DecodeError::InvalidProtobuf)?;
    validation::validate_batch(&batch, direction).map_err(DecodeError::Validation)?;
    Ok(batch)
}

// A block has no encoded length/end marker. Walk the entire input and enforce
// exact output, including overlapping matches, instead of accepting a decoder
// that stops once its output buffer is full and ignores trailing input.
fn decode_lz4(mut input: &[u8], length: usize) -> Result<Vec<u8>, DecodeError> {
    let mut output = Vec::with_capacity(length);
    let mut had_match = false;
    loop {
        let (&token, rest) = input.split_first().ok_or(DecodeError::InvalidBlock)?;
        input = rest;
        let literals = block_length(&mut input, usize::from(token >> 4))?;
        let (literal, rest) = input
            .split_at_checked(literals)
            .ok_or(DecodeError::InvalidBlock)?;
        if output.len().saturating_add(literals) > length {
            return Err(DecodeError::LengthMismatch);
        }
        output.extend_from_slice(literal);
        input = rest;
        if input.is_empty() {
            // Raw LZ4 ends with five literals, except a literal-only block.
            if had_match && literals < 5 {
                return Err(DecodeError::InvalidBlock);
            }
            return if output.len() == length {
                Ok(output)
            } else {
                Err(DecodeError::LengthMismatch)
            };
        }
        let (offset, rest) = input.split_at_checked(2).ok_or(DecodeError::InvalidBlock)?;
        let offset = usize::from(u16::from_le_bytes(
            offset.try_into().map_err(|_| DecodeError::InvalidBlock)?,
        ));
        input = rest;
        if offset == 0 || offset > output.len() {
            return Err(DecodeError::InvalidBlock);
        }
        // A match starts at least twelve decoded bytes before block end.
        if length.saturating_sub(output.len()) < 12 {
            return Err(DecodeError::InvalidBlock);
        }
        had_match = true;
        let matched = block_length(&mut input, usize::from(token & 15))?
            .checked_add(4)
            .ok_or(DecodeError::InvalidBlock)?;
        if output.len().saturating_add(matched) > length {
            return Err(DecodeError::LengthMismatch);
        }
        for _ in 0..matched {
            let index = output
                .len()
                .checked_sub(offset)
                .ok_or(DecodeError::InvalidBlock)?;
            let byte = *output.get(index).ok_or(DecodeError::InvalidBlock)?;
            output.push(byte);
        }
    }
}

fn block_length(input: &mut &[u8], base: usize) -> Result<usize, DecodeError> {
    if base != 15 {
        return Ok(base);
    }
    let mut length = base;
    loop {
        let (&byte, rest) = input.split_first().ok_or(DecodeError::InvalidBlock)?;
        *input = rest;
        length = length
            .checked_add(usize::from(byte))
            .ok_or(DecodeError::InvalidBlock)?;
        if length > MAX_DECODED_SIZE {
            return Err(DecodeError::DecodedSize);
        }
        if byte != 255 {
            return Ok(length);
        }
    }
}

#[cfg(test)]
mod tests;
