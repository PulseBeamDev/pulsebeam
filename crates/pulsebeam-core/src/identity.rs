//! Canonical PulseBeam entity identities.

use base32::Alphabet;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};
use std::{fmt, str::FromStr};
use uuid::{Uuid, Variant, Version};

const UUID_TEXT_LEN: usize = 26;
const EXTERNAL_ID_MAX_LEN: usize = 36;
const VERSION: u8 = b'0';

// These domain bytes and TrackKind discriminants are part of the V0 transcript.
const ROOM_DOMAIN: &[u8] = b"pulsebeam.identity.room.v0";
const PARTICIPANT_DOMAIN: &[u8] = b"pulsebeam.identity.participant.v0";
const AUDIO_TRACK_DOMAIN: &[u8] = b"pulsebeam.identity.audio-track.v0";
const VIDEO_TRACK_DOMAIN: &[u8] = b"pulsebeam.identity.video-track.v0";
const DATA_TRACK_DOMAIN: &[u8] = b"pulsebeam.identity.data-track.v0";

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdValidationError {
    #[error("ID is empty")]
    Empty,
    #[error("ID exceeds maximum length of {0}")]
    TooLong(usize),
    #[error("ID contains invalid characters")]
    InvalidCharacters,
    #[error("invalid ID prefix; expected {expected}")]
    InvalidPrefix { expected: &'static str },
    #[error("invalid ID length; expected {expected}, got {actual}")]
    InvalidLength { expected: usize, actual: usize },
    #[error("unsupported ID encoding version")]
    UnsupportedVersion,
    #[error("invalid Crockford Base32 encoding")]
    InvalidEncoding,
    #[error("invalid UUID variant")]
    InvalidUuidVariant,
    #[error("invalid UUID version")]
    InvalidUuidVersion,
}

fn encode_uuid(uuid: Uuid) -> String {
    base32::encode(Alphabet::Crockford, uuid.as_bytes())
}

fn decode_uuid(value: &str) -> Result<Uuid, IdValidationError> {
    if value.len() != UUID_TEXT_LEN {
        return Err(IdValidationError::InvalidLength {
            expected: UUID_TEXT_LEN,
            actual: value.len(),
        });
    }
    if !value.is_ascii() {
        return Err(IdValidationError::InvalidEncoding);
    }
    let normalized: String = value
        .bytes()
        .map(|byte| match byte.to_ascii_uppercase() {
            b'O' => '0',
            b'I' | b'L' => '1',
            byte => char::from(byte),
        })
        .collect();
    let decoded = base32::decode(Alphabet::Crockford, &normalized)
        .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
        .ok_or(IdValidationError::InvalidEncoding)?;
    let uuid = Uuid::from_bytes(decoded);
    if encode_uuid(uuid) != normalized {
        return Err(IdValidationError::InvalidEncoding);
    }
    Ok(uuid)
}

fn format_id(prefix: &str, uuid: Uuid) -> String {
    let mut value = String::with_capacity(prefix.len().saturating_add(UUID_TEXT_LEN + 2));
    value.push_str(prefix);
    value.push('_');
    value.push(char::from(VERSION));
    value.push_str(&encode_uuid(uuid));
    value
}

fn parse_id(
    value: &str,
    prefix: &'static str,
    expected_version: Version,
) -> Result<Uuid, IdValidationError> {
    let Some(encoded) = value
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
    else {
        return Err(IdValidationError::InvalidPrefix { expected: prefix });
    };
    let Some(payload) = encoded
        .strip_prefix(char::from(VERSION))
        .or_else(|| encoded.strip_prefix('O'))
        .or_else(|| encoded.strip_prefix('o'))
    else {
        return Err(IdValidationError::UnsupportedVersion);
    };
    let uuid = decode_uuid(payload)?;
    if uuid.get_variant() != Variant::RFC4122 {
        return Err(IdValidationError::InvalidUuidVariant);
    }
    if uuid.get_version() != Some(expected_version) {
        return Err(IdValidationError::InvalidUuidVersion);
    }
    Ok(uuid)
}

fn write_string(hasher: &mut Sha3_256, value: &str) {
    let length = u32::try_from(value.len()).unwrap_or(u32::MAX);
    hasher.update(length.to_be_bytes());
    hasher.update(value.as_bytes());
}

fn derive_v8(domain: &[u8], parent: &[u8; 16], kind: Option<TrackKind>, text: &str) -> Uuid {
    let mut hasher = Sha3_256::new();
    hasher.update(domain);
    hasher.update(parent);
    if let Some(kind) = kind {
        hasher.update([kind as u8]);
    }
    write_string(&mut hasher, text);
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    for (output, input) in bytes.iter_mut().zip(digest.iter()) {
        *output = *input;
    }
    if let Some(version) = bytes.get_mut(6) {
        *version = (*version & 0x0f) | 0x80;
    }
    if let Some(variant) = bytes.get_mut(8) {
        *variant = (*variant & 0x3f) | 0x80;
    }
    Uuid::from_bytes(bytes)
}

fn validate_external(value: &str) -> Result<(), IdValidationError> {
    if value.is_empty() {
        return Err(IdValidationError::Empty);
    }
    if value.len() > EXTERNAL_ID_MAX_LEN {
        return Err(IdValidationError::TooLong(EXTERNAL_ID_MAX_LEN));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(IdValidationError::InvalidCharacters);
    }
    Ok(())
}

macro_rules! external_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "&str")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: &str) -> Result<Self, IdValidationError> {
                validate_external(value)?;
                Ok(Self(value.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = IdValidationError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdValidationError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdValidationError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(&value)
            }
        }
    };
}

external_id!(RoomExternalId);
external_id!(ParticipantExternalId);

macro_rules! canonical_id {
    ($name:ident, $prefix:literal, $version:expr) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Uuid);

        impl $name {
            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }

            pub fn as_str(&self) -> String {
                format_id($prefix, self.0)
            }

            #[doc(hidden)]
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(Uuid::from_bytes(bytes))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.as_str())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = IdValidationError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_id(value, $prefix, $version).map(Self)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdValidationError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdValidationError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(&self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

canonical_id!(ProjectId, "p", Version::SortRand);
canonical_id!(RoomId, "rm", Version::Custom);
canonical_id!(ParticipantId, "pa", Version::Custom);
canonical_id!(ConnectionId, "c", Version::SortRand);
canonical_id!(ApiKeyId, "kid", Version::SortRand);
canonical_id!(AudioTrackId, "aud", Version::Custom);
canonical_id!(VideoTrackId, "vid", Version::Custom);
canonical_id!(DataTrackId, "dat", Version::Custom);

macro_rules! minted_id {
    ($name:ident) => {
        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

minted_id!(ProjectId);
minted_id!(ConnectionId);
minted_id!(ApiKeyId);

impl RoomId {
    pub fn derive(project: &ProjectId, external: &RoomExternalId) -> Self {
        Self(derive_v8(
            ROOM_DOMAIN,
            project.as_bytes(),
            None,
            external.as_str(),
        ))
    }

    #[doc(hidden)]
    pub fn from_external(external: &RoomExternalId) -> Self {
        const LEGACY_PROJECT: ProjectId = ProjectId::from_bytes([
            0x01, 0x8f, 0x4f, 0x7c, 0x20, 0x00, 0x70, 0x00, 0x80, 0x00, 0, 0, 0, 0, 0, 1,
        ]);
        Self::derive(&LEGACY_PROJECT, external)
    }
}

impl ParticipantId {
    pub fn derive(room: &RoomId, external: &ParticipantExternalId) -> Self {
        Self(derive_v8(
            PARTICIPANT_DOMAIN,
            room.as_bytes(),
            None,
            external.as_str(),
        ))
    }

    #[doc(hidden)]
    pub fn new() -> Self {
        let mut bytes = *Uuid::now_v7().as_bytes();
        if let Some(version) = bytes.get_mut(6) {
            *version = (*version & 0x0f) | 0x80;
        }
        Self(Uuid::from_bytes(bytes))
    }

    pub fn derive_track_id(&self, kind: TrackKind, label: &str) -> TrackId {
        match kind {
            TrackKind::Audio => AudioTrackId::derive(self, label).into(),
            TrackKind::Video => VideoTrackId::derive(self, label).into(),
            TrackKind::Data => DataTrackId::derive(self, label).into(),
        }
    }
}

impl Default for ParticipantId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum TrackKind {
    Audio = 0,
    Video = 1,
    Data = 2,
}

macro_rules! derived_track_id {
    ($name:ident, $kind:expr, $domain:ident) => {
        impl $name {
            pub fn derive(participant: &ParticipantId, label: &str) -> Self {
                Self(derive_v8(
                    $domain,
                    participant.as_bytes(),
                    Some($kind),
                    label,
                ))
            }
        }
    };
}

derived_track_id!(AudioTrackId, TrackKind::Audio, AUDIO_TRACK_DOMAIN);
derived_track_id!(VideoTrackId, TrackKind::Video, VIDEO_TRACK_DOMAIN);
derived_track_id!(DataTrackId, TrackKind::Data, DATA_TRACK_DOMAIN);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackId {
    Audio(AudioTrackId),
    Video(VideoTrackId),
    Data(DataTrackId),
}

impl TrackId {
    pub fn kind(self) -> TrackKind {
        match self {
            Self::Audio(_) => TrackKind::Audio,
            Self::Video(_) => TrackKind::Video,
            Self::Data(_) => TrackKind::Data,
        }
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        match self {
            Self::Audio(id) => id.as_bytes(),
            Self::Video(id) => id.as_bytes(),
            Self::Data(id) => id.as_bytes(),
        }
    }

    pub fn as_str(&self) -> String {
        match self {
            Self::Audio(id) => id.as_str(),
            Self::Video(id) => id.as_str(),
            Self::Data(id) => id.as_str(),
        }
    }
}

impl fmt::Display for TrackId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_str())
    }
}

impl fmt::Debug for TrackId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("TrackId")
            .field(&self.as_str())
            .finish()
    }
}

impl FromStr for TrackId {
    type Err = IdValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.starts_with("aud_") {
            value.parse().map(Self::Audio)
        } else if value.starts_with("vid_") {
            value.parse().map(Self::Video)
        } else if value.starts_with("dat_") {
            value.parse().map(Self::Data)
        } else {
            Err(IdValidationError::InvalidPrefix {
                expected: "aud, vid, or dat",
            })
        }
    }
}

impl TryFrom<String> for TrackId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl TryFrom<&str> for TrackId {
    type Error = IdValidationError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl Serialize for TrackId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for TrackId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl From<AudioTrackId> for TrackId {
    fn from(value: AudioTrackId) -> Self {
        Self::Audio(value)
    }
}

impl From<VideoTrackId> for TrackId {
    fn from(value: VideoTrackId) -> Self {
        Self::Video(value)
    }
}

impl From<DataTrackId> for TrackId {
    fn from(value: DataTrackId) -> Self {
        Self::Data(value)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used
    )]

    use super::*;
    use proptest::prelude::*;

    fn project(mut bytes: [u8; 16]) -> ProjectId {
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        ProjectId::from_bytes(bytes)
    }

    #[test]
    fn golden_vectors_cover_every_prefix_version_and_uuid_contract() {
        let project = project([0; 16]);
        let room_external = RoomExternalId::new("general").unwrap();
        let participant_external = ParticipantExternalId::new("alice").unwrap();
        let room = RoomId::derive(&project, &room_external);
        let participant = ParticipantId::derive(&room, &participant_external);
        let values = [
            project.as_str(),
            room.as_str(),
            participant.as_str(),
            ConnectionId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1])
                .as_str(),
            ApiKeyId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 2]).as_str(),
            AudioTrackId::derive(&participant, "mic").as_str(),
            VideoTrackId::derive(&participant, "camera").as_str(),
            DataTrackId::derive(&participant, "chat").as_str(),
        ];
        let prefixes = [
            "p_0", "rm_0", "pa_0", "c_0", "kid_0", "aud_0", "vid_0", "dat_0",
        ];
        let expected = [
            "p_00000000001R010000000000000",
            "rm_0VPDPRVA2HP4TD2HXJYXBZH53JM",
            "pa_0K2GMTHV9ME6YXAA5MPQ7GV7534",
            "c_00000000001R010000000000004",
            "kid_00000000001R010000000000008",
            "aud_0WZMFN45BYJ1Y75VMH96X5J91A0",
            "vid_00XAABXHBV6369BHJ2YMAHT8W64",
            "dat_0QDC3TZCNQ26H7340YX0N5XRMPG",
        ];
        for ((value, prefix), expected) in values.iter().zip(prefixes).zip(expected) {
            assert!(value.starts_with(prefix));
            assert_eq!(value.len(), prefix.len() + UUID_TEXT_LEN);
            assert_eq!(value, expected);
        }
        assert_eq!(
            Uuid::from_bytes(*project.as_bytes()).get_version(),
            Some(Version::SortRand)
        );
        assert_eq!(
            Uuid::from_bytes(*room.as_bytes()).get_version(),
            Some(Version::Custom)
        );
        assert_eq!(
            Uuid::from_bytes(*room.as_bytes()).get_variant(),
            Variant::RFC4122
        );
    }

    #[test]
    fn parsing_accepts_aliases_and_lowercase_then_canonicalizes() {
        let canonical = project([0; 16]).as_str();
        let lowercase = canonical.to_ascii_lowercase();
        assert_eq!(lowercase.parse::<ProjectId>().unwrap().as_str(), canonical);
        let zero_aliases = canonical.replace('0', "O");
        assert_eq!(
            zero_aliases.parse::<ProjectId>().unwrap().as_str(),
            canonical
        );

        let ending_in_one = project([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]).as_str();
        let one_alias = ending_in_one.replacen('1', "L", 1);
        assert_eq!(
            one_alias.parse::<ProjectId>().unwrap().as_str(),
            ending_in_one
        );
    }

    #[test]
    fn parsing_rejects_bad_padding_length_version_prefix_and_uuid_bits() {
        let valid = project([0; 16]).as_str();
        assert!(matches!(
            valid.replacen("p_0", "rm_0", 1).parse::<ProjectId>(),
            Err(IdValidationError::InvalidPrefix { .. })
        ));
        assert!(matches!(
            valid.replacen("p_0", "p_1", 1).parse::<ProjectId>(),
            Err(IdValidationError::UnsupportedVersion)
        ));
        assert!(matches!(
            valid.strip_suffix('0').unwrap().parse::<ProjectId>(),
            Err(IdValidationError::InvalidLength { .. })
        ));
        let invalid_padding = format!("{}Z", valid.strip_suffix('0').unwrap());
        assert!(matches!(
            invalid_padding.parse::<ProjectId>(),
            Err(IdValidationError::InvalidEncoding)
        ));
        assert!(matches!(
            valid.replacen('R', "U", 1).parse::<ProjectId>(),
            Err(IdValidationError::InvalidEncoding)
        ));
        let wrong_version = format_id("p", Uuid::from_bytes([0; 16]));
        assert!(matches!(
            wrong_version.parse::<ProjectId>(),
            Err(IdValidationError::InvalidUuidVariant) | Err(IdValidationError::InvalidUuidVersion)
        ));
    }

    #[test]
    fn derivation_is_hierarchical_case_sensitive_and_unambiguous() {
        let project_a = project([1; 16]);
        let project_b = project([2; 16]);
        let room_ab = RoomId::derive(&project_a, &RoomExternalId::new("ab").unwrap());
        let room_a = RoomId::derive(&project_a, &RoomExternalId::new("a").unwrap());
        assert_ne!(
            room_ab,
            RoomId::derive(&project_b, &RoomExternalId::new("ab").unwrap())
        );

        let participant_c =
            ParticipantId::derive(&room_ab, &ParticipantExternalId::new("c").unwrap());
        let participant_bc =
            ParticipantId::derive(&room_a, &ParticipantExternalId::new("bc").unwrap());
        assert_ne!(participant_c, participant_bc);
        assert_eq!(
            participant_c,
            ParticipantId::derive(&room_ab, &ParticipantExternalId::new("c").unwrap())
        );
        assert_ne!(
            participant_c,
            ParticipantId::derive(&room_ab, &ParticipantExternalId::new("C").unwrap())
        );
        assert_ne!(
            participant_c.derive_track_id(TrackKind::Audio, "main"),
            participant_c.derive_track_id(TrackKind::Video, "main")
        );
        assert_ne!(
            participant_c.derive_track_id(TrackKind::Video, "main"),
            participant_c.derive_track_id(TrackKind::Video, "Main")
        );
        assert_ne!(
            participant_c.derive_track_id(TrackKind::Video, "main"),
            participant_bc.derive_track_id(TrackKind::Video, "main")
        );
    }

    #[test]
    fn external_ids_enforce_ascii_contract_boundaries() {
        let max = "x".repeat(36);
        for valid in ["a", "Z", "0", "_", "-", "aB_9-z", &max] {
            assert!(RoomExternalId::new(valid).is_ok());
            assert!(ParticipantExternalId::new(valid).is_ok());
        }
        let too_long = "x".repeat(37);
        for invalid in ["", "has space", "slash/", "unicodé", &too_long] {
            assert!(RoomExternalId::new(invalid).is_err());
            assert!(ParticipantExternalId::new(invalid).is_err());
        }
    }

    #[test]
    fn minted_ids_are_monotonic_v7() {
        let first = ConnectionId::new();
        let second = ConnectionId::new();
        assert!(first < second);
        assert!(ProjectId::new() < ProjectId::new());
        assert!(ApiKeyId::new() < ApiKeyId::new());
        let uuid = Uuid::from_bytes(*first.as_bytes());
        assert_eq!(uuid.get_version(), Some(Version::SortRand));
        assert_eq!(uuid.get_variant(), Variant::RFC4122);
    }

    #[test]
    fn serde_and_kind_erasure_round_trip() {
        let project = project([9; 16]);
        let encoded = serde_json::to_string(&project).unwrap();
        assert_eq!(
            serde_json::from_str::<ProjectId>(&encoded).unwrap(),
            project
        );

        let room = RoomId::derive(&project, &RoomExternalId::new("room").unwrap());
        let participant =
            ParticipantId::derive(&room, &ParticipantExternalId::new("person").unwrap());
        for track in [
            participant.derive_track_id(TrackKind::Audio, "label"),
            participant.derive_track_id(TrackKind::Video, "label"),
            participant.derive_track_id(TrackKind::Data, "label"),
        ] {
            assert_eq!(track.as_str().parse::<TrackId>().unwrap(), track);
        }
    }

    proptest! {
        #[test]
        fn codec_round_trips_all_uuid_payloads(value in any::<u128>()) {
            let encoded = encode_uuid(Uuid::from_u128(value));
            prop_assert_eq!(decode_uuid(&encoded).unwrap().as_u128(), value);
        }
    }
}
