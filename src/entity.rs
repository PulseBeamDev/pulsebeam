use blake3;
use bs58;
use rand::RngCore;

pub type EntityId = String;
pub const E_API_KEY_ID: &str = "kid";
pub const E_API_PUBLIC_KEY: &str = "pk";
pub const E_API_SECRET: &str = "sk";
pub const E_PROJECT_ID: &str = "prj";
pub const E_GROUP_ID: &str = "grp";
pub const E_PEER_ID: &str = "peer";
pub const E_USER_ID: &str = "usr";

/// Generates a new random entity ID with optional prefix and length (in bytes).
pub fn new_random_id(prefix: &str, length: usize) -> EntityId {
    let mut bytes = vec![0u8; length];
    rand::rng().fill_bytes(&mut bytes);
    encode_with_prefix(prefix, &bytes)
}

/// Generates a new entity ID by hashing input and encoding the first `length` bytes.
pub fn new_hashed_id(prefix: &str, input: &str, length: usize) -> EntityId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(input.as_bytes());
    let mut output = vec![0u8; length];
    hasher.finalize_xof().fill(&mut output);
    encode_with_prefix(prefix, &output)
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
        let id = new_random_id("u", 16);
        assert!(id.starts_with("u_"));
        let decoded = decode_id(&id).unwrap();
        assert_eq!(decoded.len(), 16);
    }

    #[test]
    fn test_hashed_id_generation() {
        let id = new_hashed_id("g", "hello world", 8);
        assert!(id.starts_with("g_"));
        let decoded = decode_id(&id).unwrap();
        assert_eq!(decoded.len(), 8);
    }

    #[test]
    fn test_get_prefix_and_encoded() {
        let id = "p_DEF123";
        assert_eq!(get_prefix(id), Some("p"));
        assert_eq!(get_encoded_part(id), Some("DEF123"));
    }

    #[test]
    fn test_as_str() {
        let id: EntityId = "pr_ABCDEFG".to_string();
        assert_eq!(as_str(&id), "pr_ABCDEFG");
    }
}
