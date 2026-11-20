//! Canonical PulseBeam entity identities.

pub use pulsebeam_auth::identity::{IdValidationError, MAX_APPLICATION_NAME_BYTES};
#[cfg(test)]
use pulsebeam_auth::identity::{UUID_TEXT_LEN, decode_uuid, encode_uuid};
use pulsebeam_auth::identity::{format_id, parse_id, validate_application_name};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};
use std::{fmt, num::NonZeroU64, str::FromStr};
#[cfg(test)]
use uuid::Variant;
use uuid::{Uuid, Version};

// These domain bytes and TrackKind discriminants are part of the V0 transcript.
const AUDIO_TRACK_DOMAIN: &[u8] = b"pulsebeam.identity.audio-track.v0";
const VIDEO_TRACK_DOMAIN: &[u8] = b"pulsebeam.identity.video-track.v0";
const DATA_TRACK_DOMAIN: &[u8] = b"pulsebeam.identity.data-track.v0";
const PUBLICATION_DOMAIN: &[u8] = b"pulsebeam.identity.publication.v1";

fn write_string(hasher: &mut Sha3_256, value: &str) {
    let length = u32::try_from(value.len()).unwrap_or(u32::MAX);
    hasher.update(length.to_be_bytes());
    hasher.update(value.as_bytes());
}

fn derive_v8(
    domain: &[u8],
    parent: &[u8; 16],
    kind: Option<TrackKind>,
    incarnation: Option<NonZeroU64>,
    text: &str,
) -> Uuid {
    let mut hasher = Sha3_256::new();
    hasher.update(domain);
    hasher.update(parent);
    if let Some(kind) = kind {
        hasher.update([kind as u8]);
    }
    if let Some(incarnation) = incarnation {
        hasher.update(incarnation.get().to_be_bytes());
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
minted_id!(NodeId);
minted_id!(ConnectionId);
minted_id!(ApiKeyId);

impl ConnectionId {
    /// Identity of a native publication lifetime. `incarnation` is its first
    /// desired revision (owned ref), not a mutable settings revision. Pausing
    /// preserves it; retirement/republishing must use a fresh incarnation.
    pub fn derive_publication_track_id(
        &self,
        kind: TrackKind,
        label: &str,
        incarnation: NonZeroU64,
    ) -> TrackId {
        let id = derive_v8(
            PUBLICATION_DOMAIN,
            self.as_bytes(),
            Some(kind),
            Some(incarnation),
            label,
        );
        match kind {
            TrackKind::Audio => AudioTrackId(id).into(),
            TrackKind::Video => VideoTrackId(id).into(),
            TrackKind::Data => DataTrackId(id).into(),
        }
    }

    /// Single bootstrap lifetime, for SDP/legacy sources without owned refs.
    pub fn derive_track_id(&self, kind: TrackKind, label: &str) -> TrackId {
        match kind {
            TrackKind::Audio => AudioTrackId::derive(self, label).into(),
            TrackKind::Video => VideoTrackId::derive(self, label).into(),
            TrackKind::Data => DataTrackId::derive(self, label).into(),
        }
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
            pub fn derive(connection: &ConnectionId, label: &str) -> Self {
                Self(derive_v8(
                    $domain,
                    connection.as_bytes(),
                    Some($kind),
                    None,
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
        let connection =
            ConnectionId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1]);
        let values = [
            project.as_str(),
            connection.as_str(),
            ApiKeyId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 2]).as_str(),
            AudioTrackId::derive(&connection, "mic").as_str(),
            VideoTrackId::derive(&connection, "camera").as_str(),
            DataTrackId::derive(&connection, "chat").as_str(),
        ];
        let prefixes = ["p_0", "conn_0", "kid_0", "aud_0", "vid_0", "dat_0"];
        let expected = [
            "p_00000000001R010000000000000",
            "conn_00000000001R010000000000004",
            "kid_00000000001R010000000000008",
            "aud_0Z91QNP7AYT6MZBQWDV1YN13G88",
            "vid_04NCB5P75CY1FN9ZPW5ZQEXDNXG",
            "dat_053TY7A2ANE1K9BKZNBWRJS46CG",
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
    fn derived_track_id_preserves_kind_and_case_and_scopes_by_connection() {
        let connection_a = ConnectionId::new();
        let connection_b = ConnectionId::new();
        assert_eq!(
            connection_a.derive_track_id(TrackKind::Video, "main"),
            connection_a.derive_track_id(TrackKind::Video, "main")
        );
        assert_ne!(
            connection_a.derive_track_id(TrackKind::Audio, "main"),
            connection_a.derive_track_id(TrackKind::Video, "main")
        );
        assert_ne!(
            connection_a.derive_track_id(TrackKind::Video, "main"),
            connection_a.derive_track_id(TrackKind::Video, "Main")
        );
        assert_ne!(
            connection_a.derive_track_id(TrackKind::Video, "main"),
            connection_b.derive_track_id(TrackKind::Video, "main")
        );
    }

    #[test]
    fn publication_lifetimes_preserve_kind_label_and_exact_u64_incarnation() {
        let connection = ConnectionId::new();
        let first = NonZeroU64::MIN;
        let next = NonZeroU64::new((1_u64 << 53) + 1).unwrap();
        let after_next = NonZeroU64::new(next.get() + 1).unwrap();
        let mut identities = std::collections::HashSet::new();
        for kind in [TrackKind::Audio, TrackKind::Video, TrackKind::Data] {
            for incarnation in [first, next, after_next, NonZeroU64::MAX] {
                let track = connection.derive_publication_track_id(kind, "main", incarnation);
                assert_eq!(
                    track,
                    connection.derive_publication_track_id(kind, "main", incarnation)
                );
                assert_eq!(track.kind(), kind);
                assert_eq!(track.as_str().parse::<TrackId>().unwrap(), track);
                assert!(identities.insert(track));
                assert_ne!(track, connection.derive_track_id(kind, "main"));
                assert_ne!(
                    track,
                    connection.derive_publication_track_id(kind, "Main", incarnation)
                );
                assert_ne!(
                    track,
                    ConnectionId::new().derive_publication_track_id(kind, "main", incarnation)
                );
            }
        }
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
            assert_eq!(room.as_str(), valid);
            assert_eq!(participant.as_str(), valid);
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
        }
        assert_ne!(RoomId::new("Stage"), RoomId::new("stage"));
        assert_ne!(ParticipantId::new("Alice"), ParticipantId::new("alice"));
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
    fn serde_and_kind_erasure_round_trip() {
        let project = project([9; 16]);
        let encoded = serde_json::to_string(&project).unwrap();
        assert_eq!(
            serde_json::from_str::<ProjectId>(&encoded).unwrap(),
            project
        );

        let connection = ConnectionId::new();
        for track in [
            connection.derive_track_id(TrackKind::Audio, "label"),
            connection.derive_track_id(TrackKind::Video, "label"),
            connection.derive_track_id(TrackKind::Data, "label"),
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
