use alloc::vec;
use alloc::vec::Vec;

use lz4_flex::block::{DecompressError, compress_into, decompress_into, get_maximum_output_size};
use prost::Message;

use crate::signaling_v1::{ClientMessage, ServerMessage};

pub const MAX_MESSAGE_SIZE: usize = 256 * 1024;
const SIZE_PREFIX_LEN: usize = size_of::<u32>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeError {
    MissingPayload,
    UncompressedTooLarge,
    CompressedTooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    CompressedTooLarge,
    TruncatedPrefix,
    AdvertisedSizeTooLarge,
    TruncatedBlock,
    OutputSizeMismatch,
    InvalidBlock,
    InvalidProtobuf,
    MissingPayload,
}

pub fn encode_client(message: &ClientMessage) -> Result<Vec<u8>, EncodeError> {
    if message.payload.is_none() {
        return Err(EncodeError::MissingPayload);
    }
    encode(message)
}

pub fn decode_client(input: &[u8]) -> Result<ClientMessage, DecodeError> {
    let message: ClientMessage = decode(input)?;
    if message.payload.is_none() {
        return Err(DecodeError::MissingPayload);
    }
    Ok(message)
}

pub fn encode_server(message: &ServerMessage) -> Result<Vec<u8>, EncodeError> {
    if message.payload.is_none() {
        return Err(EncodeError::MissingPayload);
    }
    encode(message)
}

pub fn decode_server(input: &[u8]) -> Result<ServerMessage, DecodeError> {
    let message: ServerMessage = decode(input)?;
    if message.payload.is_none() {
        return Err(DecodeError::MissingPayload);
    }
    Ok(message)
}

fn encode<M: Message>(message: &M) -> Result<Vec<u8>, EncodeError> {
    if message.encoded_len() > MAX_MESSAGE_SIZE {
        return Err(EncodeError::UncompressedTooLarge);
    }

    let protobuf = message.encode_to_vec();
    let output_capacity = SIZE_PREFIX_LEN
        .checked_add(get_maximum_output_size(protobuf.len()))
        .ok_or(EncodeError::CompressedTooLarge)?
        .min(MAX_MESSAGE_SIZE);
    let mut output = vec![0; output_capacity];
    let prefix = u32::try_from(protobuf.len())
        .map_err(|_| EncodeError::UncompressedTooLarge)?
        .to_le_bytes();
    output
        .get_mut(..SIZE_PREFIX_LEN)
        .ok_or(EncodeError::CompressedTooLarge)?
        .copy_from_slice(&prefix);

    let compressed = output
        .get_mut(SIZE_PREFIX_LEN..)
        .ok_or(EncodeError::CompressedTooLarge)?;
    let compressed_len =
        compress_into(&protobuf, compressed).map_err(|_| EncodeError::CompressedTooLarge)?;
    let output_len = SIZE_PREFIX_LEN
        .checked_add(compressed_len)
        .ok_or(EncodeError::CompressedTooLarge)?;
    output.truncate(output_len);
    Ok(output)
}

fn decode<M: Message + Default>(input: &[u8]) -> Result<M, DecodeError> {
    if input.len() > MAX_MESSAGE_SIZE {
        return Err(DecodeError::CompressedTooLarge);
    }

    let prefix: [u8; SIZE_PREFIX_LEN] = input
        .get(..SIZE_PREFIX_LEN)
        .ok_or(DecodeError::TruncatedPrefix)?
        .try_into()
        .map_err(|_| DecodeError::TruncatedPrefix)?;
    let advertised_len = usize::try_from(u32::from_le_bytes(prefix))
        .map_err(|_| DecodeError::AdvertisedSizeTooLarge)?;
    if advertised_len > MAX_MESSAGE_SIZE {
        return Err(DecodeError::AdvertisedSizeTooLarge);
    }

    let block = input
        .get(SIZE_PREFIX_LEN..)
        .ok_or(DecodeError::TruncatedPrefix)?;
    let mut protobuf = vec![0; advertised_len];
    let actual_len = decompress_into(block, &mut protobuf).map_err(map_decompress_error)?;
    if actual_len != advertised_len {
        return Err(DecodeError::OutputSizeMismatch);
    }

    M::decode(protobuf.as_slice()).map_err(|_| DecodeError::InvalidProtobuf)
}

fn map_decompress_error(error: DecompressError) -> DecodeError {
    match error {
        DecompressError::ExpectedAnotherByte | DecompressError::LiteralOutOfBounds => {
            DecodeError::TruncatedBlock
        }
        DecompressError::OutputTooSmall { .. } => DecodeError::OutputSizeMismatch,
        _ => DecodeError::InvalidBlock,
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used
)]
mod tests {
    use alloc::string::String;
    use alloc::vec;
    use lz4_flex::block::compress_prepend_size;

    use super::*;
    use crate::signaling_v1::{
        AudioIntent, Authorization, Error as ProtocolError, Intent, Mapping, ReceiveIntent,
        RenewAuthorization, SendIntent, TrackMappings, VideoIntent, client_message, server_message,
    };

    fn empty_intent() -> ClientMessage {
        ClientMessage {
            payload: Some(client_message::Payload::Intent(Intent {
                revision: 1,
                send: Some(SendIntent { tracks: vec![] }),
                receive: Some(ReceiveIntent {
                    video: Some(VideoIntent { tracks: vec![] }),
                    audio: Some(AudioIntent {
                        tracks: vec![],
                        mode: 0,
                    }),
                }),
            })),
        }
    }

    fn empty_mapping() -> ServerMessage {
        ServerMessage {
            payload: Some(server_message::Payload::Mapping(Mapping {
                intent_revision: 1,
                video: Some(TrackMappings { tracks: vec![] }),
                audio: Some(TrackMappings { tracks: vec![] }),
            })),
        }
    }

    #[test]
    fn client_and_server_round_trip_empty_replacements() {
        let client = empty_intent();
        let client_wire = encode_client(&client).expect("client message fits");
        assert_eq!(decode_client(&client_wire), Ok(client));

        let server = empty_mapping();
        let server_wire = encode_server(&server).expect("server message fits");
        assert_eq!(decode_server(&server_wire), Ok(server));
    }

    #[test]
    fn framing_matches_reference_bytes() {
        let message = ClientMessage {
            payload: Some(client_message::Payload::RenewAuthorization(
                RenewAuthorization {
                    token: String::from("a"),
                },
            )),
        };

        assert_eq!(
            encode_client(&message),
            Ok(vec![5, 0, 0, 0, 0x50, 0x12, 0x03, 0x0a, 0x01, b'a'])
        );
    }

    #[test]
    fn rejects_truncated_prefix_and_block() {
        assert_eq!(decode_client(&[0, 0, 0]), Err(DecodeError::TruncatedPrefix));

        let mut wire = encode_client(&empty_intent()).expect("client message fits");
        wire.pop();
        assert_eq!(decode_client(&wire), Err(DecodeError::TruncatedBlock));
    }

    #[test]
    fn rejects_false_output_size_and_invalid_block() {
        let mut wire = encode_client(&empty_intent()).expect("client message fits");
        let advertised = u32::from_le_bytes(wire.get(..4).unwrap().try_into().unwrap());
        wire.get_mut(..4)
            .unwrap()
            .copy_from_slice(&advertised.saturating_add(1).to_le_bytes());
        assert_eq!(decode_client(&wire), Err(DecodeError::OutputSizeMismatch));

        let invalid = [4, 0, 0, 0, 0, 0, 0];
        assert_eq!(decode_client(&invalid), Err(DecodeError::InvalidBlock));
    }

    #[test]
    fn rejects_invalid_protobuf_and_missing_payload() {
        assert_eq!(
            decode_client(&compress_prepend_size(&[0xff])),
            Err(DecodeError::InvalidProtobuf)
        );
        assert_eq!(
            decode_client(&compress_prepend_size(&[])),
            Err(DecodeError::MissingPayload)
        );
        assert_eq!(
            encode_client(&ClientMessage { payload: None }),
            Err(EncodeError::MissingPayload)
        );
        assert_eq!(
            encode_server(&ServerMessage { payload: None }),
            Err(EncodeError::MissingPayload)
        );
    }

    #[test]
    fn enforces_decode_limits_before_decompression() {
        assert_eq!(
            decode_client(&vec![0; MAX_MESSAGE_SIZE + 1]),
            Err(DecodeError::CompressedTooLarge)
        );

        let advertised = u32::try_from(MAX_MESSAGE_SIZE + 1)
            .expect("protocol limit fits u32")
            .to_le_bytes();
        assert_eq!(
            decode_server(&advertised),
            Err(DecodeError::AdvertisedSizeTooLarge)
        );
    }

    #[test]
    fn rejects_compressible_oversized_protobuf_on_both_sides() {
        let token = "a".repeat(MAX_MESSAGE_SIZE);
        let client = ClientMessage {
            payload: Some(client_message::Payload::RenewAuthorization(
                RenewAuthorization {
                    token: token.clone(),
                },
            )),
        };
        assert_eq!(
            encode_client(&client),
            Err(EncodeError::UncompressedTooLarge)
        );

        let server = ServerMessage {
            payload: Some(server_message::Payload::Error(ProtocolError {
                code: 0,
                message: token,
                fatal: false,
                intent_revision: None,
            })),
        };
        assert_eq!(
            encode_server(&server),
            Err(EncodeError::UncompressedTooLarge)
        );
    }

    #[test]
    fn server_round_trip_uses_the_same_codec() {
        let server = ServerMessage {
            payload: Some(server_message::Payload::Authorization(Authorization {
                expires_at_unix_seconds: i64::MAX,
            })),
        };
        let wire = encode_server(&server).expect("small server message fits");
        assert_eq!(decode_server(&wire), Ok(server));
    }

    #[test]
    fn rejects_incompressible_block_that_exceeds_wire_limit() {
        let mut state = 0x1234_5678_u32;
        let token: String = (0..MAX_MESSAGE_SIZE - 16)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                u8::try_from(33 + state % 94).expect("value is printable ASCII")
            })
            .map(char::from)
            .collect();
        let message = ClientMessage {
            payload: Some(client_message::Payload::RenewAuthorization(
                RenewAuthorization { token },
            )),
        };
        assert!(message.encoded_len() <= MAX_MESSAGE_SIZE);
        assert_eq!(
            encode_client(&message),
            Err(EncodeError::CompressedTooLarge)
        );
    }
}
