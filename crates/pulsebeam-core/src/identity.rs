//! Canonical PulseBeam entity identities.

pub use pulsebeam_auth::identity::{IdValidationError, MAX_APPLICATION_NAME_BYTES};
#[cfg(test)]
use pulsebeam_auth::identity::{UUID_TEXT_LEN, decode_uuid, encode_uuid};
use pulsebeam_auth::identity::{format_id, parse_id, validate_application_name};
use serde::{Deserialize, Serialize};
#[cfg(any(test, feature = "test-utils"))]
use sha3::{Digest, Sha3_256};
#[cfg(any(test, feature = "test-utils"))]
use std::num::NonZeroU64;
use std::{fmt, str::FromStr};
use uuid::Variant;
use uuid::{Uuid, Version};

macro_rules! application_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name {
            bytes: [u8; MAX_APPLICATION_NAME_BYTES],
            len: u8,
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct(stringify!($name))
                    .finish_non_exhaustive()
            }
        }

        impl $name {
            pub fn new(value: &str) -> Result<Self, IdValidationError> {
                validate_application_name(value)?;
                let mut bytes = [0; MAX_APPLICATION_NAME_BYTES];
                for (output, input) in bytes.iter_mut().zip(value.bytes()) {
                    *output = input;
                }
                let len = u8::try_from(value.len())
                    .map_err(|_| IdValidationError::TooLong(MAX_APPLICATION_NAME_BYTES))?;
                Ok(Self { bytes, len })
            }

            pub fn as_str(&self) -> &str {
                // The constructor admits only ASCII and records its exact length.
                self.bytes
                    .get(..usize::from(self.len))
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .unwrap_or_default()
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
        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

application_id!(RoomId);
application_id!(ParticipantId);
application_id!(TrackId);

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
canonical_id!(NodeId, "n", Version::SortRand);
canonical_id!(ConnectionId, "conn", Version::SortRand);
canonical_id!(PublicationId, "pub", Version::SortRand);
canonical_id!(ApiKeyId, "kid", Version::SortRand);

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
minted_id!(NodeId);
minted_id!(ConnectionId);
minted_id!(ApiKeyId);

impl PublicationId {
    pub fn mint() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn try_from_bytes(bytes: [u8; 16]) -> Result<Self, IdValidationError> {
        let uuid = Uuid::from_bytes(bytes);
        if uuid.get_variant() != Variant::RFC4122 {
            return Err(IdValidationError::InvalidUuidVariant);
        }
        if uuid.get_version() != Some(Version::SortRand) {
            return Err(IdValidationError::InvalidUuidVersion);
        }
        Ok(Self(uuid))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum TrackKind {
    Audio = 0,
    Video = 1,
    Data = 2,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum PublicationKey {
    Audio(PublicationId),
    Video(PublicationId),
    Data(PublicationId),
}

impl PublicationKey {
    pub const fn new(kind: TrackKind, id: PublicationId) -> Self {
        match kind {
            TrackKind::Audio => Self::Audio(id),
            TrackKind::Video => Self::Video(id),
            TrackKind::Data => Self::Data(id),
        }
    }

    pub const fn kind(self) -> TrackKind {
        match self {
            Self::Audio(_) => TrackKind::Audio,
            Self::Video(_) => TrackKind::Video,
            Self::Data(_) => TrackKind::Data,
        }
    }

    pub const fn publication_id(self) -> PublicationId {
        match self {
            Self::Audio(id) | Self::Video(id) | Self::Data(id) => id,
        }
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        match self {
            Self::Audio(id) | Self::Video(id) | Self::Data(id) => id.as_bytes(),
        }
    }

    pub fn as_str(&self) -> String {
        self.publication_id().as_str()
    }
}

impl fmt::Display for PublicationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.publication_id(), formatter)
    }
}

impl fmt::Debug for PublicationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PublicationKey")
            .field(&self.kind())
            .field(&self.publication_id())
            .finish()
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl ConnectionId {
    pub fn fixture_publication(&self, kind: TrackKind, name: &str) -> PublicationKey {
        self.fixture_publication_at(kind, name, NonZeroU64::MIN)
    }

    pub fn fixture_publication_at(
        &self,
        kind: TrackKind,
        name: &str,
        incarnation: NonZeroU64,
    ) -> PublicationKey {
        let mut hash = Sha3_256::new();
        hash.update(b"pulsebeam.fixture.publication");
        hash.update(self.as_bytes());
        hash.update([kind as u8]);
        hash.update(incarnation.get().to_be_bytes());
        hash.update(name.as_bytes());
        let digest = hash.finalize();
        let mut bytes = [0u8; 16];
        for (byte, input) in bytes.iter_mut().zip(digest.iter()) {
            *byte = *input;
        }
        if let Some(version) = bytes.get_mut(6) {
            *version = (*version & 0x0f) | 0x70;
        }
        if let Some(variant) = bytes.get_mut(8) {
            *variant = (*variant & 0x3f) | 0x80;
        }
        PublicationKey::new(kind, PublicationId::from_bytes(bytes))
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
        let connection =
            ConnectionId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1]);
        let values = [
            project.as_str(),
            connection.as_str(),
            ApiKeyId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 2]).as_str(),
            PublicationId::from_bytes(*connection.as_bytes()).as_str(),
        ];
        let prefixes = ["p_0", "conn_0", "kid_0", "pub_0"];
        let expected = [
            "p_00000000001R010000000000000",
            "conn_00000000001R010000000000004",
            "kid_00000000001R010000000000008",
            "pub_00000000001R010000000000004",
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
    }

    #[test]
    fn connection_id_rejects_legacy_prefix() {
        let legacy = "c_00000000001R010000000000004";
        assert!(matches!(
            legacy.parse::<ConnectionId>(),
            Err(IdValidationError::InvalidPrefix { expected: "conn" })
        ));
        assert!(serde_json::from_str::<ConnectionId>(&format!("\"{legacy}\"")).is_err());
    }

    #[test]
    fn publication_id_round_trips_as_a_distinct_version_seven_identity() {
        let id = PublicationId::mint();
        let text = id.as_str();
        assert!(text.starts_with("pub_0"));
        assert_eq!(text.parse::<PublicationId>().unwrap(), id);
        assert_eq!(
            serde_json::from_str::<PublicationId>(&serde_json::to_string(&id).unwrap()).unwrap(),
            id
        );
        assert!(text.parse::<ConnectionId>().is_err());
        assert!(
            ConnectionId::new()
                .as_str()
                .parse::<PublicationId>()
                .is_err()
        );
        let legacy = "vid_04NCB5P75CY1FN9ZPW5ZQEXDNXG";
        assert!(legacy.parse::<PublicationId>().is_err());
        let wrong_version = legacy.replacen("vid_", "pub_", 1);
        assert!(matches!(
            wrong_version.parse::<PublicationId>(),
            Err(IdValidationError::InvalidUuidVersion)
        ));
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
    fn publication_reference_does_not_encode_the_media_kind() {
        let id = PublicationId::mint();
        let audio = PublicationKey::new(TrackKind::Audio, id);
        let video = PublicationKey::new(TrackKind::Video, id);
        assert_ne!(audio, video);
        assert_eq!(audio.as_str().parse(), Ok(id));
        assert_eq!(video.as_str().parse(), Ok(id));
        assert_ne!(id, PublicationId::mint());
    }

    #[test]
    fn application_name_debug_does_not_expose_names() {
        let room = RoomId::new("Room_Sentinel").unwrap();
        let participant = ParticipantId::new("Participant_Sentinel").unwrap();
        let debug = format!("{room:?} {participant:?}");
        assert!(!debug.contains("Sentinel"));
    }

    #[test]
    fn application_names_round_trip_without_case_folding() {
        let longest = "a".repeat(MAX_APPLICATION_NAME_BYTES);
        for valid in ["A", "z", "0", "_", "-", "Stage_9-a", &longest] {
            let room = RoomId::new(valid).unwrap();
            let participant = ParticipantId::new(valid).unwrap();
            let track = TrackId::new(valid).unwrap();
            assert_eq!(room.as_str(), valid);
            assert_eq!(participant.as_str(), valid);
            assert_eq!(track.as_str(), valid);
            assert_eq!(
                serde_json::from_str::<RoomId>(&serde_json::to_string(&room).unwrap()).unwrap(),
                room,
            );
            assert_eq!(
                serde_json::from_str::<ParticipantId>(
                    &serde_json::to_string(&participant).unwrap()
                )
                .unwrap(),
                participant,
            );
            assert_eq!(
                serde_json::from_str::<TrackId>(&serde_json::to_string(&track).unwrap()).unwrap(),
                track,
            );
        }
        assert_ne!(RoomId::new("Stage"), RoomId::new("stage"));
        assert_ne!(ParticipantId::new("Alice"), ParticipantId::new("alice"));
        assert_ne!(TrackId::new("Main"), TrackId::new("main"));
        for invalid in [
            "",
            "stage.main",
            "room/main",
            " café",
            "abc\n",
            &format!("{longest}x"),
        ] {
            assert!(RoomId::new(invalid).is_err());
            assert!(ParticipantId::new(invalid).is_err());
        }
    }

    #[test]
    fn node_identity_uses_the_shared_v0_v7_codec() {
        let id = NodeId::new();
        let next = NodeId::new();
        assert!(next > id);
        let encoded = id.to_string();
        assert!(encoded.starts_with("n_0"));
        assert_eq!(encoded.parse::<NodeId>().unwrap(), id);
        assert_eq!(
            serde_json::from_str::<NodeId>(&serde_json::to_string(&id).unwrap()).unwrap(),
            id
        );
        assert!(encoded.parse::<ProjectId>().is_err());
        assert!(ProjectId::new().to_string().parse::<NodeId>().is_err());
        assert!(encoded.replacen("n_0", "n_1", 1).parse::<NodeId>().is_err());
        let mut bytes = *id.as_bytes();
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        assert!(
            NodeId::from_bytes(bytes)
                .to_string()
                .parse::<NodeId>()
                .is_err()
        );
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
        bytes[8] = 0;
        assert!(
            NodeId::from_bytes(bytes)
                .to_string()
                .parse::<NodeId>()
                .is_err()
        );
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
    fn project_serde_round_trip() {
        let project = project([9; 16]);
        let encoded = serde_json::to_string(&project).unwrap();
        assert_eq!(
            serde_json::from_str::<ProjectId>(&encoded).unwrap(),
            project
        );
    }

    proptest! {
        #[test]
        fn codec_round_trips_all_uuid_payloads(value in any::<u128>()) {
            let encoded = encode_uuid(Uuid::from_u128(value));
            prop_assert_eq!(decode_uuid(&encoded).unwrap().as_u128(), value);
        }
    }
}
