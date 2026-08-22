use std::fmt;

use aes_gcm::aead::AeadInPlace;
use aes_gcm::aead::consts::U12;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha256;

pub const E2EE_FRAME_VERSION: u8 = 2;
const HEADER_LEN: usize = 29;
const TAG_LEN: usize = 16;
const REPLAY_WORDS: usize = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct E2eeMasterKey {
    pub key_id: u32,
    pub bytes: [u8; 32],
}

impl E2eeMasterKey {
    pub const fn new(key_id: u32, bytes: [u8; 32]) -> Self {
        Self { key_id, bytes }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct E2eeEpoch(pub [u8; 16]);

impl E2eeEpoch {
    pub fn new(bytes: [u8; 16]) -> Result<Self, E2eeError> {
        if bytes == [0; 16] {
            return Err(E2eeError::InvalidEpoch);
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum E2eeDirection {
    Send,
    Receive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct E2eeDomain {
    pub sender: String,
    pub stream: String,
    pub direction: E2eeDirection,
}

impl E2eeDomain {
    pub fn new(
        sender: impl Into<String>,
        stream: impl Into<String>,
        direction: E2eeDirection,
    ) -> Result<Self, E2eeError> {
        let sender = sender.into();
        let stream = stream.into();
        if sender.is_empty() || stream.is_empty() {
            return Err(E2eeError::InvalidDomain);
        }
        Ok(Self {
            sender,
            stream,
            direction,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct E2eeFrame {
    pub key_id: u32,
    pub epoch: E2eeEpoch,
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
        output.extend_from_slice(&self.epoch.0);
        output.extend_from_slice(&self.counter.to_be_bytes());
        output.extend_from_slice(&self.ciphertext);
        output.extend_from_slice(&self.tag);
        output
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, E2eeError> {
        if bytes.len() < HEADER_LEN + TAG_LEN {
            return Err(E2eeError::InvalidFrame);
        }
        let header = bytes.get(..HEADER_LEN).ok_or(E2eeError::InvalidFrame)?;
        let version = header.first().copied().ok_or(E2eeError::InvalidFrame)?;
        if version != E2EE_FRAME_VERSION {
            return Err(E2eeError::UnsupportedVersion(version));
        }
        let key_id = u32::from_be_bytes(
            header
                .get(1..5)
                .ok_or(E2eeError::InvalidFrame)?
                .try_into()
                .map_err(|_| E2eeError::InvalidFrame)?,
        );
        let epoch = E2eeEpoch::new(
            header
                .get(5..21)
                .ok_or(E2eeError::InvalidFrame)?
                .try_into()
                .map_err(|_| E2eeError::InvalidFrame)?,
        )?;
        let counter = u64::from_be_bytes(
            header
                .get(21..29)
                .ok_or(E2eeError::InvalidFrame)?
                .try_into()
                .map_err(|_| E2eeError::InvalidFrame)?,
        );
        let ciphertext_end = bytes
            .len()
            .checked_sub(TAG_LEN)
            .ok_or(E2eeError::InvalidFrame)?;
        let ciphertext = bytes
            .get(HEADER_LEN..ciphertext_end)
            .ok_or(E2eeError::InvalidFrame)?
            .to_vec();
        let tag = bytes
            .get(ciphertext_end..)
            .ok_or(E2eeError::InvalidFrame)?
            .try_into()
            .map_err(|_| E2eeError::InvalidFrame)?;
        Ok(Self {
            key_id,
            epoch,
            counter,
            ciphertext,
            tag,
        })
    }

    fn header(&self) -> [u8; HEADER_LEN] {
        let mut header = [0; HEADER_LEN];
        header[0] = E2EE_FRAME_VERSION;
        header[1..5].copy_from_slice(&self.key_id.to_be_bytes());
        header[5..21].copy_from_slice(&self.epoch.0);
        header[21..].copy_from_slice(&self.counter.to_be_bytes());
        header
    }
}

pub struct E2eeKeyRing {
    entries: Vec<KeyEntry>,
    max_receive_epochs: usize,
}

struct KeyEntry {
    key: E2eeMasterKey,
    epoch: E2eeEpoch,
    domain: E2eeDomain,
    derived: [u8; 32],
}

impl E2eeKeyRing {
    pub fn new(max_receive_epochs: usize) -> Result<Self, E2eeError> {
        if max_receive_epochs == 0 {
            return Err(E2eeError::InvalidEpochLimit);
        }
        Ok(Self {
            entries: Vec::new(),
            max_receive_epochs,
        })
    }

    pub fn install(
        &mut self,
        key: E2eeMasterKey,
        epoch: E2eeEpoch,
        domain: E2eeDomain,
    ) -> Result<(), E2eeError> {
        let derived = derive_key(&key, epoch, &domain)?;
        self.entries.retain(|entry| {
            !(entry.key.key_id == key.key_id && entry.epoch == epoch && entry.domain == domain)
        });
        self.entries.push(KeyEntry {
            key,
            epoch,
            domain,
            derived,
        });
        while self.entries.len() > self.max_receive_epochs {
            self.entries.remove(0);
        }
        Ok(())
    }

    pub fn retire(&mut self, key_id: u32, epoch: E2eeEpoch, domain: &E2eeDomain) {
        self.entries.retain(|entry| {
            !(entry.key.key_id == key_id && entry.epoch == epoch && &entry.domain == domain)
        });
    }

    pub fn encryptor(
        &self,
        key_id: u32,
        epoch: E2eeEpoch,
        domain: &E2eeDomain,
    ) -> Result<E2eeEncryptor, E2eeError> {
        let entry = self.find(key_id, epoch, domain)?;
        Ok(E2eeEncryptor {
            key_id,
            epoch,
            cipher: cipher(entry.derived)?,
            counter: 0,
        })
    }

    pub fn receiver(
        &self,
        key_id: u32,
        epoch: E2eeEpoch,
        domain: &E2eeDomain,
    ) -> Result<E2eeReceiver, E2eeError> {
        let entry = self.find(key_id, epoch, domain)?;
        Ok(E2eeReceiver {
            key_id,
            epoch,
            cipher: cipher(entry.derived)?,
            replay: ReplayWindow::default(),
        })
    }

    fn find(
        &self,
        key_id: u32,
        epoch: E2eeEpoch,
        domain: &E2eeDomain,
    ) -> Result<&KeyEntry, E2eeError> {
        self.entries
            .iter()
            .find(|entry| {
                entry.key.key_id == key_id && entry.epoch == epoch && &entry.domain == domain
            })
            .ok_or(E2eeError::UnknownKey(key_id))
    }
}

pub struct E2eeEncryptor {
    key_id: u32,
    epoch: E2eeEpoch,
    cipher: Aes256Gcm,
    counter: u64,
}

impl E2eeEncryptor {
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, E2eeError> {
        let counter = self.counter;
        let next = counter.checked_add(1).ok_or(E2eeError::CounterExhausted)?;
        let frame = E2eeFrame {
            key_id: self.key_id,
            epoch: self.epoch,
            counter,
            ciphertext: Vec::new(),
            tag: [0; TAG_LEN],
        };
        let mut ciphertext = plaintext.to_vec();
        let tag = self
            .cipher
            .encrypt_in_place_detached(&nonce(counter), &frame.header(), &mut ciphertext)
            .map_err(|_| E2eeError::SealFailed)?;
        let tag_bytes: [u8; TAG_LEN] = tag.into();
        self.counter = next;
        Ok(E2eeFrame {
            ciphertext,
            tag: tag_bytes,
            ..frame
        }
        .encode())
    }
}

pub struct E2eeReceiver {
    key_id: u32,
    epoch: E2eeEpoch,
    cipher: Aes256Gcm,
    replay: ReplayWindow,
}

impl E2eeReceiver {
    pub fn decrypt(&mut self, bytes: &[u8]) -> Result<Vec<u8>, E2eeError> {
        let frame = E2eeFrame::decode(bytes)?;
        if frame.key_id != self.key_id || frame.epoch != self.epoch {
            return Err(E2eeError::UnknownKey(frame.key_id));
        }
        if !self.replay.may_accept(frame.counter) {
            return Err(E2eeError::Replay(frame.counter));
        }
        let header = frame.header();
        let counter = frame.counter;
        let mut plaintext = frame.ciphertext;
        let tag = Tag::from(frame.tag);
        self.cipher
            .decrypt_in_place_detached(&nonce(counter), &header, &mut plaintext, &tag)
            .map_err(|_| E2eeError::OpenFailed)?;
        self.replay.accept(frame.counter);
        Ok(plaintext)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ReplayWindow {
    highest: Option<u64>,
    bits: [u64; REPLAY_WORDS],
}

impl ReplayWindow {
    fn may_accept(&self, counter: u64) -> bool {
        let Some(highest) = self.highest else {
            return true;
        };
        if counter > highest {
            return true;
        }
        let distance = highest.saturating_sub(counter);
        if distance >= 128 {
            return false;
        }
        let index = usize::try_from(distance / 64).unwrap_or(usize::MAX);
        let Some(bits) = self.bits.get(index) else {
            debug_assert!(false, "replay-window word index must remain in bounds");
            return false;
        };
        bits & (1_u64 << (distance % 64)) == 0
    }

    fn accept(&mut self, counter: u64) {
        let Some(highest) = self.highest else {
            self.highest = Some(counter);
            self.bits[0] = 1;
            return;
        };
        if counter > highest {
            let shift = counter.saturating_sub(highest);
            if shift >= 128 {
                self.bits = [0; REPLAY_WORDS];
            } else {
                let shift = usize::try_from(shift).unwrap_or(usize::MAX);
                shift_bits(&mut self.bits, shift);
            }
            self.highest = Some(counter);
            self.bits[0] |= 1;
        } else {
            let distance = usize::try_from(highest.saturating_sub(counter)).unwrap_or(usize::MAX);
            if distance < 128 {
                let index = distance / 64;
                let Some(bits) = self.bits.get_mut(index) else {
                    debug_assert!(false, "replay-window word index must remain in bounds");
                    return;
                };
                *bits |= 1_u64 << (distance % 64);
            }
        }
    }
}

fn shift_bits(bits: &mut [u64; REPLAY_WORDS], shift: usize) {
    let word_shift = shift / 64;
    let bit_shift = shift % 64;
    for index in (0..REPLAY_WORDS).rev() {
        let value = index.checked_sub(word_shift).map_or(0, |source| {
            let Some(&source_bits) = bits.get(source) else {
                debug_assert!(false, "replay-window source index must remain in bounds");
                return 0;
            };
            let mut value = source_bits << bit_shift;
            if bit_shift != 0 && source > 0 {
                let Some(&previous_bits) = bits.get(source.saturating_sub(1)) else {
                    debug_assert!(false, "replay-window previous index must remain in bounds");
                    return value;
                };
                value |= previous_bits >> 64_usize.saturating_sub(bit_shift);
            }
            value
        });
        let Some(slot) = bits.get_mut(index) else {
            debug_assert!(
                false,
                "replay-window destination index must remain in bounds"
            );
            continue;
        };
        *slot = value;
    }
}

fn derive_key(
    key: &E2eeMasterKey,
    epoch: E2eeEpoch,
    domain: &E2eeDomain,
) -> Result<[u8; 32], E2eeError> {
    let mut info = Vec::new();
    info.extend_from_slice(b"pulsebeam-agent-e2ee-v2");
    push_string(&mut info, &domain.sender)?;
    push_string(&mut info, &domain.stream)?;
    info.push(match domain.direction {
        E2eeDirection::Send => 0,
        E2eeDirection::Receive => 1,
    });
    info.extend_from_slice(&key.key_id.to_be_bytes());
    info.extend_from_slice(&epoch.0);
    let hkdf = Hkdf::<Sha256>::new(Some(&epoch.0), &key.bytes);
    let mut derived = [0; 32];
    hkdf.expand(&info, &mut derived)
        .map_err(|_| E2eeError::KeyDerivationFailed)?;
    Ok(derived)
}

fn push_string(output: &mut Vec<u8>, value: &str) -> Result<(), E2eeError> {
    let length = u32::try_from(value.len()).map_err(|_| E2eeError::InvalidDomain)?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn cipher(key: [u8; 32]) -> Result<Aes256Gcm, E2eeError> {
    Aes256Gcm::new_from_slice(&key).map_err(|_| E2eeError::InvalidKey)
}

fn nonce(counter: u64) -> Nonce<U12> {
    let mut bytes = [0; 12];
    bytes[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(bytes)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum E2eeError {
    InvalidKey,
    InvalidFrame,
    UnsupportedVersion(u8),
    InvalidEpoch,
    InvalidEpochLimit,
    InvalidDomain,
    UnknownKey(u32),
    Replay(u64),
    CounterExhausted,
    KeyDerivationFailed,
    SealFailed,
    OpenFailed,
}

impl fmt::Display for E2eeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey => f.write_str("invalid AES-256-GCM key"),
            Self::InvalidFrame => f.write_str("invalid E2EE frame"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported E2EE version {v}"),
            Self::InvalidEpoch => f.write_str("invalid E2EE epoch"),
            Self::InvalidEpochLimit => f.write_str("invalid E2EE epoch limit"),
            Self::InvalidDomain => f.write_str("invalid E2EE domain"),
            Self::UnknownKey(id) => write!(f, "unknown E2EE key {id}"),
            Self::Replay(c) => write!(f, "replayed E2EE counter {c}"),
            Self::CounterExhausted => f.write_str("E2EE counter exhausted"),
            Self::KeyDerivationFailed => f.write_str("E2EE key derivation failed"),
            Self::SealFailed => f.write_str("E2EE encryption failed"),
            Self::OpenFailed => f.write_str("E2EE authentication failed"),
        }
    }
}

impl std::error::Error for E2eeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (E2eeKeyRing, E2eeDomain, E2eeEpoch) {
        let key = E2eeMasterKey::new(7, [0x11; 32]);
        let epoch = E2eeEpoch::new([0x22; 16]).expect("nonzero epoch");
        let domain = E2eeDomain::new("sender", "stream", E2eeDirection::Send).expect("domain");
        let mut ring = E2eeKeyRing::new(4).expect("limit");
        ring.install(key, epoch, domain.clone()).expect("key");
        (ring, domain, epoch)
    }

    #[test]
    fn round_trip_and_reordering() {
        let (ring, domain, epoch) = fixture();
        let mut sender = ring.encryptor(7, epoch, &domain).expect("sender");
        let mut receiver = ring.receiver(7, epoch, &domain).expect("receiver");
        let first = sender.encrypt(b"one").expect("first");
        let second = sender.encrypt(b"two").expect("second");
        assert_eq!(receiver.decrypt(&second).expect("second decrypt"), b"two");
        assert_eq!(receiver.decrypt(&first).expect("reordered decrypt"), b"one");
        assert_eq!(receiver.decrypt(&first), Err(E2eeError::Replay(0)));
    }
}
