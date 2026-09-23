use alloc::vec;
use alloc::vec::Vec;

use lz4_flex::block::{DecompressError, compress_into, decompress_into, get_maximum_output_size};
use prost::Message;

use crate::signaling_v1::{ClientMessage, ServerMessage};

pub const MAX_MESSAGE_SIZE: usize = 32_768;
pub const MAX_COMPRESSED_SIZE: usize = 32_912;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeError {
    MissingPayload,
    UncompressedTooLarge,
    CompressedTooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    CompressedTooLarge,
    TruncatedBlock,
    OutputTooLarge,
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
    let mut output = vec![0; get_maximum_output_size(protobuf.len()).min(MAX_COMPRESSED_SIZE)];
    let compressed_len =
        compress_into(&protobuf, &mut output).map_err(|_| EncodeError::CompressedTooLarge)?;
    output.truncate(compressed_len);
    Ok(output)
}

fn decode<M: Message + Default>(input: &[u8]) -> Result<M, DecodeError> {
    if input.len() > MAX_COMPRESSED_SIZE {
        return Err(DecodeError::CompressedTooLarge);
    }

    let mut protobuf = vec![0; MAX_MESSAGE_SIZE];
    let actual_len = decompress_into(input, &mut protobuf).map_err(map_decompress_error)?;
    protobuf.truncate(actual_len);
    M::decode(protobuf.as_slice()).map_err(|_| DecodeError::InvalidProtobuf)
}

fn map_decompress_error(error: DecompressError) -> DecodeError {
    match error {
        DecompressError::ExpectedAnotherByte | DecompressError::LiteralOutOfBounds => {
            DecodeError::TruncatedBlock
        }
        DecompressError::OutputTooSmall { .. } => DecodeError::OutputTooLarge,
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
    use lz4_flex::block::{compress, decompress_into};

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
    fn superseded_terminal_reason_is_machine_readable() {
        let message = ServerMessage {
            payload: Some(server_message::Payload::Error(ProtocolError {
                code: crate::signaling_v1::ErrorCode::Superseded.into(),
                message: String::new(),
                fatal: true,
                intent_revision: None,
            })),
        };
        let bytes = encode_server(&message).expect("superseded terminal fits");
        assert_eq!(decode_server(&bytes), Ok(message));
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
            Ok(vec![0x50, 0x12, 0x03, 0x0a, 0x01, b'a'])
        );
    }

    #[test]
    fn raw_block_matches_independent_decoder_and_rejects_truncation() {
        let wire = encode_client(&empty_intent()).expect("client message fits");
        let mut output = vec![0; MAX_MESSAGE_SIZE];
        let decoded = decompress_into(&wire, &mut output).expect("independent decoder");
        assert_eq!(
            ClientMessage::decode(&output[..decoded]),
            Ok(empty_intent())
        );
        assert_eq!(
            decode_client(&wire[..wire.len() - 1]),
            Err(DecodeError::TruncatedBlock)
        );
        assert_eq!(decode_client(&[0, 0, 0]), Err(DecodeError::InvalidBlock));
    }

    #[test]
    fn rejects_invalid_protobuf_and_missing_payload() {
        assert_eq!(
            decode_client(&compress(&[0xff])),
            Err(DecodeError::InvalidProtobuf)
        );
        assert_eq!(
            decode_client(&compress(&[])),
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
            decode_client(&vec![0; MAX_COMPRESSED_SIZE + 1]),
            Err(DecodeError::CompressedTooLarge)
        );

        assert_eq!(
            decode_server(&compress(&vec![0; MAX_MESSAGE_SIZE + 1])),
            Err(DecodeError::OutputTooLarge)
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
