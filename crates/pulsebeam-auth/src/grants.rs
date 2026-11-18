//! Bounded, transport-free authorization scopes shared by signers and verification.

use core::fmt;
use serde::{Deserialize, Deserializer, Serialize, de};

pub const MAX_GRANTS: usize = 32;
pub const MAX_ROOM_PATTERNS: usize = 64;
pub use crate::MAX_SCOPE_BYTES;
const MAX_ACTIONS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Join,
    Publish,
    Discover,
    Subscribe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Audio,
    Video,
    Data,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    #[error("authorization exceeds the grant limit")]
    TooManyGrants,
    #[error("authorization exceeds the room pattern limit")]
    TooManyRoomPatterns,
    #[error("a grant requires at least one room pattern")]
    EmptyRooms,
    #[error("invalid room pattern")]
    InvalidRoomPattern,
    #[error("a grant requires one to four distinct actions")]
    InvalidActions,
    #[error("invalid authorization filter")]
    InvalidFilter,
    #[error("authorization filters are incompatible with an action")]
    InvalidActionFilters,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
struct RoomPattern(String);

impl RoomPattern {
    fn new(value: String) -> Result<Self, GrantError> {
        if !valid_scope(&value)
            || (value.contains('*')
                && value != "*"
                && !value
                    .strip_suffix('*')
                    .is_some_and(|prefix| prefix.ends_with('/') && !prefix.contains('*')))
        {
            return Err(GrantError::InvalidRoomPattern);
        }
        Ok(Self(value))
    }

    fn matches(&self, room: &str) -> bool {
        self.0 == "*"
            || self.0 == room
            || self
                .0
                .strip_suffix('*')
                .is_some_and(|prefix| room.starts_with(prefix))
    }
}

/// Validated grant. Construction and deserialization enforce the same rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Grant {
    rooms: Vec<RoomPattern>,
    allow: Vec<Action>,
    #[serde(skip_serializing_if = "Option::is_none")]
    participant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<TrackKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

impl Grant {
    pub fn new(
        rooms: Vec<String>,
        allow: Vec<Action>,
        participant: Option<String>,
        kind: Option<TrackKind>,
        label: Option<String>,
    ) -> Result<Self, GrantError> {
        if rooms.is_empty() {
            return Err(GrantError::EmptyRooms);
        }
        if rooms.len() > MAX_ROOM_PATTERNS {
            return Err(GrantError::TooManyRoomPatterns);
        }
        if allow.is_empty()
            || allow.len() > MAX_ACTIONS
            || allow
                .iter()
                .enumerate()
                .any(|(index, action)| allow.iter().take(index).any(|prior| prior == action))
        {
            return Err(GrantError::InvalidActions);
        }
        if participant.as_deref().is_some_and(|s| !valid_scope(s))
            || label.as_deref().is_some_and(|s| !valid_scope(s))
        {
            return Err(GrantError::InvalidFilter);
        }
        let has_filters = participant.is_some() || kind.is_some() || label.is_some();
        if (allow.contains(&Action::Join) && has_filters)
            || (allow.contains(&Action::Publish) && participant.is_some())
        {
            return Err(GrantError::InvalidActionFilters);
        }
        Ok(Self {
            rooms: rooms
                .into_iter()
                .map(RoomPattern::new)
                .collect::<Result<_, _>>()?,
            allow,
            participant,
            kind,
            label,
        })
    }

    fn matches(
        &self,
        action: Action,
        room: &str,
        participant: Option<&str>,
        kind: Option<TrackKind>,
        label: Option<&str>,
    ) -> bool {
        self.allow.contains(&action)
            && self.rooms.iter().any(|pattern| pattern.matches(room))
            && self
                .participant
                .as_deref()
                .is_none_or(|filter| participant == Some(filter))
            && self.kind.is_none_or(|filter| kind == Some(filter))
            && self
                .label
                .as_deref()
                .is_none_or(|filter| label == Some(filter))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantWire {
    rooms: BoundedVec<String, MAX_ROOM_PATTERNS>,
    allow: BoundedVec<String, MAX_ACTIONS>,
    #[serde(default, deserialize_with = "present_filter")]
    participant: Option<String>,
    #[serde(default, deserialize_with = "present_filter")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "present_filter")]
    label: Option<String>,
}

// Omission is unrestricted; explicit null is not a filter value in the schema.
fn present_filter<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for Grant {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Grant;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a grant object")
            }
            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Grant, M::Error> {
                let wire = GrantWire::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Grant::try_from(wire).map_err(de::Error::custom)
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

impl TryFrom<GrantWire> for Grant {
    type Error = GrantError;

    fn try_from(wire: GrantWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.rooms.0,
            wire.allow
                .0
                .into_iter()
                .map(|action| match action.as_str() {
                    "join" => Ok(Action::Join),
                    "publish" => Ok(Action::Publish),
                    "discover" => Ok(Action::Discover),
                    "subscribe" => Ok(Action::Subscribe),
                    _ => Err(GrantError::InvalidActions),
                })
                .collect::<Result<Vec<_>, _>>()?,
            wire.participant,
            wire.kind
                .map(|kind| match kind.as_str() {
                    "audio" => Ok(TrackKind::Audio),
                    "video" => Ok(TrackKind::Video),
                    "data" => Ok(TrackKind::Data),
                    _ => Err(GrantError::InvalidFilter),
                })
                .transpose()?,
            wire.label,
        )
    }
}

/// Additive grants, checked before any normalization or deduplication.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct GrantSet(Vec<Grant>);

impl GrantSet {
    pub fn new(grants: Vec<Grant>) -> Result<Self, GrantError> {
        if grants.len() > MAX_GRANTS {
            return Err(GrantError::TooManyGrants);
        }
        if grants.iter().map(|grant| grant.rooms.len()).sum::<usize>() > MAX_ROOM_PATTERNS {
            return Err(GrantError::TooManyRoomPatterns);
        }
        Ok(Self(grants))
    }

    /// Preflight an unresolved subscription, without authorizing receipt.
    /// Call `allows` with the resolved publisher, kind and label before execution.
    pub fn may_subscribe(&self, room: &str, kind: TrackKind) -> bool {
        valid_scope(room)
            && !room.contains('*')
            && self.0.iter().any(|grant| {
                grant.allow.contains(&Action::Subscribe)
                    && grant.rooms.iter().any(|pattern| pattern.matches(room))
                    && grant.kind.is_none_or(|filter| filter == kind)
            })
    }

    /// Project room-local publication label scopes for bounded admission compilation.
    /// Each result is an exact label, or `None` for unrestricted labels.
    pub fn publication_labels<'a>(
        &'a self,
        room: &'a str,
        kind: TrackKind,
    ) -> impl Iterator<Item = Option<&'a str>> {
        let valid_room = valid_scope(room) && !room.contains('*');
        self.0
            .iter()
            .filter(move |grant| {
                valid_room
                    && grant.matches(
                        Action::Publish,
                        room,
                        None,
                        Some(kind),
                        grant.label.as_deref(),
                    )
            })
            .map(|grant| grant.label.as_deref())
    }

    /// Match one action and concrete target. Missing target fields cannot match filters.
    /// This grants no resource budget and does not infer discovery from subscription.
    pub fn allows(
        &self,
        action: Action,
        room: &str,
        participant: Option<&str>,
        kind: Option<TrackKind>,
        label: Option<&str>,
    ) -> bool {
        valid_scope(room)
            && !room.contains('*')
            && participant.is_none_or(valid_scope)
            && label.is_none_or(valid_scope)
            && self
                .0
                .iter()
                .any(|grant| grant.matches(action, room, participant, kind, label))
    }
}

impl<'de> Deserialize<'de> for GrantSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let grants = BoundedVec::<Grant, MAX_GRANTS>::deserialize(deserializer)?;
        Self::new(grants.0).map_err(de::Error::custom)
    }
}

fn valid_scope(value: &str) -> bool {
    crate::identity::validate_external(value).is_ok()
}

struct BoundedVec<T, const MAX: usize>(Vec<T>);

struct ExcessEntry;

impl<'de> Deserialize<'de> for ExcessEntry {
    fn deserialize<D: Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(de::Error::custom("authorization array exceeds its limit"))
    }
}

impl<'de, T: Deserialize<'de>, const MAX: usize> Deserialize<'de> for BoundedVec<T, MAX> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor<T, const MAX: usize>(core::marker::PhantomData<T>);

        impl<'de, T: Deserialize<'de>, const MAX: usize> de::Visitor<'de> for Visitor<T, MAX> {
            type Value = BoundedVec<T, MAX>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "an array with at most {MAX} entries")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while values.len() < MAX {
                    let Some(value) = sequence.next_element()? else {
                        return Ok(BoundedVec(values));
                    };
                    values.push(value);
                }
                // Reject an excess element without traversing its untrusted shape.
                let _ = sequence.next_element::<ExcessEntry>()?;
                Ok(BoundedVec(values))
            }
        }

        deserializer.deserialize_seq(Visitor::<T, MAX>(core::marker::PhantomData))
    }
}

#[cfg(test)]
mod tests;
