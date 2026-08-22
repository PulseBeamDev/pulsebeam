use std::fmt;

use aws_lc_rs::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};

pub const E2EE_FRAME_VERSION: u8 = 1;
const HEADER_LEN: usize = 13;
const TAG_LEN: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct E2eeKey {
    pub key_id: u32,
    pub bytes: [u8; 32],
}

impl E2eeKey {
    pub const fn new(key_id: u32, bytes: [u8; 32]) -> Self {
        Self { key_id, bytes }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct E2eeFrame {
    pub key_id: u32,
    pub counter: u64,
    pub ciphertext: Vec<u8>,
    pub tag: [u8; TAG_LEN],
}

impl E2eeFrame {
    pub fn encode(&self) -> Vec<u8> {
        let capacity = HEADER_LEN
            .saturating_add(self.ciphertext.len())
            .saturating_add(TAG_LEN);
        let mut output = Vec::with_capacity(capacity);
        output.push(E2EE_FRAME_VERSION);
        output.extend_from_slice(&self.key_id.to_be_bytes());
        output.extend_from_slice(&self.counter.to_be_bytes());
        output.extend_from_slice(&self.ciphertext);
        output.extend_from_slice(&self.tag);
        output
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, E2eeError> {
        let minimum = HEADER_LEN
            .checked_add(TAG_LEN)
            .ok_or(E2eeError::InvalidFrame)?;
        if bytes.len() < minimum {
            return Err(E2eeError::InvalidFrame);
        }
        let header = bytes.get(..HEADER_LEN).ok_or(E2eeError::InvalidFrame)?;
        let version = header.first().copied().ok_or(E2eeError::InvalidFrame)?;
        if version != E2EE_FRAME_VERSION {
            return Err(E2eeError::UnsupportedVersion(version));
        }
        let key_id_bytes = header.get(1..5).ok_or(E2eeError::InvalidFrame)?;
        let counter_bytes = header.get(5..HEADER_LEN).ok_or(E2eeError::InvalidFrame)?;
        let mut key_id = [0u8; 4];
        key_id.copy_from_slice(key_id_bytes);
        let mut counter = [0u8; 8];
        counter.copy_from_slice(counter_bytes);
        let ciphertext_end = bytes
            .len()
            .checked_sub(TAG_LEN)
            .ok_or(E2eeError::InvalidFrame)?;
        let ciphertext = bytes
            .get(HEADER_LEN..ciphertext_end)
            .ok_or(E2eeError::InvalidFrame)?
            .to_vec();
        let tag_bytes = bytes.get(ciphertext_end..).ok_or(E2eeError::InvalidFrame)?;
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(tag_bytes);
        Ok(Self {
            key_id: u32::from_be_bytes(key_id),
            counter: u64::from_be_bytes(counter),
            ciphertext,
            tag,
        })
    }

    fn header(&self) -> [u8; HEADER_LEN] {
        let mut header = [0u8; HEADER_LEN];
        if let Some(version) = header.first_mut() {
            *version = E2EE_FRAME_VERSION;
        }
        if let Some(key_id) = header.get_mut(1..5) {
            key_id.copy_from_slice(&self.key_id.to_be_bytes());
        }
        if let Some(counter) = header.get_mut(5..HEADER_LEN) {
            counter.copy_from_slice(&self.counter.to_be_bytes());
        }
        header
    }
}

pub struct E2eeSession {
    key_id: u32,
    key: LessSafeKey,
    send_counter: u64,
    highest_received: Option<u64>,
}

impl E2eeSession {
    pub fn new(key: E2eeKey) -> Result<Self, E2eeError> {
        let unbound =
            UnboundKey::new(&aead::AES_256_GCM, &key.bytes).map_err(|_| E2eeError::InvalidKey)?;
        Ok(Self {
            key_id: key.key_id,
            key: LessSafeKey::new(unbound),
            send_counter: 0,
            highest_received: None,
        })
    }

    pub fn key_id(&self) -> u32 {
        self.key_id
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, E2eeError> {
        let counter = self.send_counter;
        self.send_counter = self
            .send_counter
            .checked_add(1)
            .ok_or(E2eeError::CounterExhausted)?;
        let mut frame = E2eeFrame {
            key_id: self.key_id,
            counter,
            ciphertext: plaintext.to_vec(),
            tag: [0u8; TAG_LEN],
        };
        let header = frame.header();
        let tag = self
            .key
            .seal_in_place_separate_tag(
                nonce(self.key_id, counter),
                Aad::from(header),
                frame.ciphertext.as_mut_slice(),
            )
            .map_err(|_| E2eeError::SealFailed)?;
        frame.tag.copy_from_slice(tag.as_ref());
        Ok(frame.encode())
    }

    pub fn decrypt(&mut self, bytes: &[u8]) -> Result<Vec<u8>, E2eeError> {
        let frame = E2eeFrame::decode(bytes)?;
        if frame.key_id != self.key_id {
            return Err(E2eeError::UnknownKey(frame.key_id));
        }
        if self
            .highest_received
            .is_some_and(|highest| frame.counter <= highest)
        {
            return Err(E2eeError::Replay(frame.counter));
        }
        let header = frame.header();
        let mut ciphertext_and_tag = frame.ciphertext;
        ciphertext_and_tag.extend_from_slice(&frame.tag);
        let plaintext = self
            .key
            .open_in_place(
                nonce(frame.key_id, frame.counter),
                Aad::from(header),
                ciphertext_and_tag.as_mut_slice(),
            )
            .map_err(|_| E2eeError::OpenFailed)?
            .to_vec();
        self.highest_received = Some(frame.counter);
        Ok(plaintext)
    }
}

fn nonce(key_id: u32, counter: u64) -> Nonce {
    let mut bytes = [0u8; aead::NONCE_LEN];
    if let Some(key) = bytes.get_mut(..4) {
        key.copy_from_slice(&key_id.to_be_bytes());
    }
    if let Some(counter_bytes) = bytes.get_mut(4..) {
        counter_bytes.copy_from_slice(&counter.to_be_bytes());
    }
    Nonce::assume_unique_for_key(bytes)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum E2eeError {
    InvalidKey,
    InvalidFrame,
    UnsupportedVersion(u8),
    UnknownKey(u32),
    Replay(u64),
    CounterExhausted,
    SealFailed,
    OpenFailed,
}

impl fmt::Display for E2eeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey => formatter.write_str("invalid AES-256-GCM key"),
            Self::InvalidFrame => formatter.write_str("invalid E2EE frame"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported E2EE version {version}")
            }
            Self::UnknownKey(key_id) => write!(formatter, "unknown E2EE key {key_id}"),
            Self::Replay(counter) => write!(formatter, "replayed E2EE counter {counter}"),
            Self::CounterExhausted => formatter.write_str("E2EE counter exhausted"),
            Self::SealFailed => formatter.write_str("E2EE encryption failed"),
            Self::OpenFailed => formatter.write_str("E2EE authentication failed"),
        }
    }
}

impl std::error::Error for E2eeError {}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn aes_gcm_frame_round_trips_and_authenticates_header() {
        let key = E2eeKey::new(7, [0x11; 32]);
        let mut sender = E2eeSession::new(key.clone()).unwrap();
        let mut receiver = E2eeSession::new(key).unwrap();
        let encoded = sender.encrypt(b"payload").unwrap();
        assert_eq!(encoded.first().copied(), Some(E2EE_FRAME_VERSION));
        assert_eq!(receiver.decrypt(&encoded).unwrap(), b"payload");
        assert_eq!(receiver.decrypt(&encoded), Err(E2eeError::Replay(0)));
        let mut tampered = encoded;
        if let Some(byte) = tampered.get_mut(1) {
            *byte ^= 1;
        }
        assert_eq!(
            receiver.decrypt(&tampered),
            Err(E2eeError::UnknownKey(16777223))
        );
    }
}
