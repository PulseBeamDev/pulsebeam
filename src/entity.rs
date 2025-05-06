use rand::RngCore;
use sha3::{Digest, Sha3_256};

pub type EntityId = String;

pub mod prefix {
    pub const API_KEY_ID: &str = "kid";
    pub const API_PUBLIC_KEY: &str = "pk";
    pub const API_SECRET: &str = "sk";
    pub const PROJECT_ID: &str = "p";
    pub const ROOM_ID: &str = "rm";
    pub const PARTICIPANT_ID: &str = "pa";
    pub const USER_ID: &str = "u";
}

const HASH_OUTPUT_BYTES: usize = 16;

/// Generates a new random entity ID with optional prefix and length (in bytes).
pub fn new_random_id(prefix: &str, length: usize) -> EntityId {
    let mut bytes = vec![0u8; length];
    rand::rng().fill_bytes(&mut bytes);
    encode_with_prefix(prefix, &bytes)
}

pub fn new_hashed_id(prefix: &str, input: &str) -> EntityId {
    let mut hasher = Sha3_256::default();
    hasher.update(input.as_bytes());
    let full_hash = hasher.finalize();
    let hash_output_slice = &full_hash[..HASH_OUTPUT_BYTES];

    encode_with_prefix(prefix, hash_output_slice)
}

/// Decodes the base58-encoded part of the ID into raw bytes.
pub fn decode_id(id: &str) -> Option<Vec<u8>> {
    id.split_once('_')
        .and_then(|(_, encoded)| bs58::decode(encoded).into_vec().ok())
}

/// Returns the prefix part (before `_`) of the ID.
pub fn get_prefix(id: &str) -> Option<&str> {
    id.split_once('_').map(|(prefix, _)| prefix)
}

/// Returns the base58 portion (after `_`) of the ID.
pub fn get_encoded_part(id: &str) -> Option<&str> {
    id.split_once('_').map(|(_, b58)| b58)
}

/// Returns the ID as a string reference (useful if passed as EntityId).
pub fn as_str(id: &EntityId) -> &str {
    id.as_str()
}

fn encode_with_prefix(prefix: &str, bytes: &[u8]) -> EntityId {
    let encoded = bs58::encode(bytes).into_string();
    format!("{}_{}", prefix, encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_random_id_generation() {
        let id = new_random_id(prefix::USER_ID, 16);
        assert!(id.starts_with("u_"));
        let decoded = decode_id(&id).unwrap();
        assert_eq!(decoded.len(), 16);
    }

    #[test]
    fn test_hashed_id_generation() {
        let id = new_hashed_id(prefix::ROOM_ID, "hello world");
        assert!(id.starts_with("rm_"));
        let decoded = decode_id(&id).unwrap();
        assert_eq!(decoded.len(), HASH_OUTPUT_BYTES);
    }

    #[test]
    fn test_get_prefix_and_encoded() {
        let id = "pa_DEF123";
        assert_eq!(get_prefix(id), Some("pa"));
        assert_eq!(get_encoded_part(id), Some("DEF123"));
    }

    #[test]
    fn test_as_str() {
        let id: EntityId = "p_ABCDEFG".to_string();
        assert_eq!(as_str(&id), "p_ABCDEFG");
    }
}
