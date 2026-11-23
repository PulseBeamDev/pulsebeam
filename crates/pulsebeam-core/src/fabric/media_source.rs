//! Identity ledger for one authorized media source incarnation ([RFC #275 §4.1][rfc]).
//!
//! The endpoint adapter must resolve primary vs RTX, validate the endpoint
//! binding and framing, strip padding and OSN, and select qualified extensions.
//! This ledger neither grants authority nor handles branch admissions or repair.
//!
//! [rfc]: https://github.com/PulseBeamDev/pulsebeam/issues/275

use std::{
    collections::BTreeMap,
    num::NonZeroU16,
    time::{Duration, Instant},
};

use arrayvec::ArrayVec;

use super::{Extension, ExtensionKind, ExtensionWidth, FabricPacket, Header, Profile, WireError};

const MAX_POSITIONS: u16 = 4096;
const SEQUENCE_CYCLE: u64 = 1 << 16;
const TIMESTAMP_CYCLE: u64 = 1 << 32;

pub struct Limits {
    /// Owner-reserved positions, at most 4096. Copy evidence retains at most
    /// one value per additional registered extension kind at each position.
    pub positions: NonZeroU16,
    /// Maximum age from authenticated ingress to this ledger, and from first
    /// admission to a competing copy. Neither limit uses RTP or NTP time.
    pub max_age: Duration,
    /// Require a new authenticated epoch after a gap longer than this.
    pub max_idle: Duration,
}

#[derive(Default)]
pub struct Anchors {
    pub sequence: Option<u64>,
    pub timestamp: Option<u64>,
}

pub struct MediaCopy<'a> {
    pub is_repair: bool,
    pub marker: bool,
    pub timestamp: u32,
    pub payload: &'a [u8],
    pub extensions: &'a [Extension<'a>],
    pub extension_width: Option<ExtensionWidth>,
}

pub enum Input<'a> {
    PrimaryPadding,
    Media(MediaCopy<'a>),
}

#[derive(Clone, Copy)]
pub enum Position {
    LowSequence(u16),
    AuthenticatedPrimary(u64),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Admission<'a> {
    Padding,
    Media {
        sequence: u64,
        first: bool,
        arrival: Instant,
        packet: &'a FabricPacket<Box<[u8]>>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    Wire(WireError),
    Ambiguous,
    PreOrigin,
    Stale,
    Epoch,
    Conflict,
    Fenced,
}

/// Standalone observer. The dataplane can use `MediaIdentity` against its
/// existing retained originals instead of maintaining an observer's store.
pub struct MediaSource {
    identity: MediaIdentity,
    originals: BTreeMap<u64, FabricPacket<Box<[u8]>>>,
}

/// Endpoint RTP identity metadata for one source. It never retains a media
/// payload. Its owner supplies the original when validating a competing copy.
pub struct MediaIdentity {
    profile: Profile,
    limits: Limits,
    high: Option<u64>,
    resume_floor: u64,
    last_progress: Option<Instant>,
    clock: Clock,
    ledger: BTreeMap<u64, Entry>,
    fenced: bool,
}

struct Entry {
    admitted_at: Instant,
    media: Option<CopyEvidence>,
}

struct CopyEvidence {
    primary_presence: Option<u16>,
    additional_values: Vec<(ExtensionKind, Box<[u8]>)>,
}

pub enum IdentityAdmission {
    Padding,
    Media {
        sequence: u64,
        arrival: Instant,
        original: Option<FabricPacket<Box<[u8]>>>,
    },
}

#[derive(Clone, Copy)]
enum Clock {
    Unanchored,
    Tracking(u64),
    Ambiguous,
}

impl MediaSource {
    pub fn new(profile: Profile, anchors: Anchors, limits: Limits) -> Option<Self> {
        Some(Self {
            identity: MediaIdentity::new(profile, anchors, limits)?,
            originals: BTreeMap::new(),
        })
    }

    pub fn into_identity(self) -> MediaIdentity {
        self.identity
    }

    pub fn extended_timestamp(&self, raw: u32) -> Option<u64> {
        self.identity.extended_timestamp(raw)
    }

    pub fn admit(
        &mut self,
        now: Instant,
        arrival: Instant,
        ingress_ntp32: u32,
        low_sequence: u16,
        input: Input<'_>,
    ) -> Result<Admission<'_>, Rejected> {
        let originals = &self.originals;
        let accepted = self.identity.admit(
            now,
            arrival,
            ingress_ntp32,
            Position::LowSequence(low_sequence),
            input,
            |sequence| originals.get(&sequence),
        )?;
        let oldest = self.identity.oldest();
        while self
            .originals
            .first_key_value()
            .is_some_and(|(&key, _)| key < oldest)
        {
            self.originals.pop_first();
        }
        Ok(match accepted {
            IdentityAdmission::Padding => Admission::Padding,
            IdentityAdmission::Media {
                sequence,
                arrival,
                original,
            } => {
                let first = original.is_some();
                if let Some(original) = original {
                    self.originals.insert(sequence, original);
                }
                Admission::Media {
                    sequence,
                    arrival,
                    first,
                    packet: self.originals.get(&sequence).ok_or(Rejected::Stale)?,
                }
            }
        })
    }
}

impl MediaIdentity {
    pub const fn profile(&self) -> Profile {
        self.profile
    }

    pub fn new(profile: Profile, anchors: Anchors, limits: Limits) -> Option<Self> {
        if profile == Profile::Data || limits.positions.get() > MAX_POSITIONS {
            return None;
        }
        Some(Self {
            profile,
            high: anchors.sequence,
            resume_floor: 0,
            clock: anchors.timestamp.map_or(Clock::Unanchored, Clock::Tracking),
            limits,
            last_progress: None,
            ledger: BTreeMap::new(),
            fenced: false,
        })
    }

    /// Expand a raw timestamp against the current media anchor. Ambiguity never
    /// modifies the frozen wire timestamp or blocks whole-source admission.
    pub fn extended_timestamp(&self, raw: u32) -> Option<u64> {
        match self.clock {
            Clock::Tracking(anchor) => unwrap(u64::from(raw), anchor, TIMESTAMP_CYCLE).ok(),
            Clock::Unanchored | Clock::Ambiguous => None,
        }
    }

    pub fn oldest(&self) -> u64 {
        self.high
            .unwrap_or_default()
            .saturating_sub(u64::from(self.limits.positions.get()).saturating_sub(1))
            .max(self.resume_floor)
    }

    /// True only when every position in a nonempty range was authenticated
    /// as primary padding for this incarnation.
    pub fn covers_padding(&self, positions: std::ops::Range<u64>) -> bool {
        if self.fenced || positions.is_empty() || positions.start < self.resume_floor {
            return false;
        }
        let count = positions.end.saturating_sub(positions.start);
        u64::try_from(
            self.ledger
                .range(positions)
                .take_while(|(_, entry)| entry.media.is_none())
                .count(),
        ) == Ok(count)
    }

    pub fn admit<'a>(
        &mut self,
        now: Instant,
        arrival: Instant,
        ingress_ntp32: u32,
        position: Position,
        input: Input<'_>,
        original: impl FnOnce(u64) -> Option<&'a FabricPacket<Box<[u8]>>>,
    ) -> Result<IdentityAdmission, Rejected> {
        if self.fenced {
            return Err(Rejected::Fenced);
        }
        if now
            .checked_duration_since(arrival)
            .is_none_or(|age| age > self.limits.max_age)
        {
            return Err(Rejected::Stale);
        }
        let idle = self
            .last_progress
            .is_some_and(|last| now.saturating_duration_since(last) > self.limits.max_idle);
        let sequence = match position {
            Position::AuthenticatedPrimary(sequence) if !matches!(&input, Input::Media(copy) if copy.is_repair) => {
                sequence
            }
            Position::LowSequence(low) if !idle => {
                let low = u64::from(low);
                self.high
                    .map_or(Ok(low), |high| unwrap(low, high, SEQUENCE_CYCLE))?
            }
            Position::AuthenticatedPrimary(_) | Position::LowSequence(_) => {
                return Err(Rejected::Epoch);
            }
        };
        // Only an advancing, replay-checked primary carrying media may resume
        // an idle binding. RTX's independent SRTP index proves no primary position.
        if idle
            && (self.high.is_none_or(|high| sequence <= high) || !matches!(&input, Input::Media(_)))
        {
            return Err(Rejected::Epoch);
        }
        if sequence < self.resume_floor {
            return Err(Rejected::Stale);
        }
        let span = u64::from(self.limits.positions.get());
        if let Some(high) = self.high {
            if high.saturating_sub(sequence) >= span {
                return Err(Rejected::Stale);
            }
            if sequence.saturating_sub(high) > u64::from(MAX_POSITIONS) {
                return Err(Rejected::Epoch);
            }
        }
        let (packet, primary) = match input {
            Input::PrimaryPadding => (None, true),
            Input::Media(copy) => {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "RFC #275 carries only the low 32 bits on wire"
                )]
                let wire_sequence = sequence as u32;
                let packet = FabricPacket::pack(
                    Header {
                        profile: self.profile,
                        marker: copy.marker,
                        sequence: wire_sequence,
                        timestamp: copy.timestamp,
                        ingress_ntp32,
                    },
                    copy.extensions,
                    copy.payload,
                    copy.extension_width,
                )
                .map_err(Rejected::Wire)?;
                (Some(packet), !copy.is_repair)
            }
        };
        if let Some(entry) = self.ledger.get_mut(&sequence) {
            if now.saturating_duration_since(entry.admitted_at) > self.limits.max_age {
                return Err(Rejected::Stale);
            }
            let accepted = match (entry.media.as_mut(), packet.as_ref()) {
                (None, None) => Some(IdentityAdmission::Padding),
                (Some(evidence), Some(copy)) => {
                    let retained = original(sequence).ok_or(Rejected::Stale)?;
                    evidence
                        .observe(retained, copy, primary)
                        .then_some(IdentityAdmission::Media {
                            sequence,
                            arrival: entry.admitted_at,
                            original: None,
                        })
                }
                _ => None,
            };
            return accepted.ok_or_else(|| {
                self.fenced = true;
                Rejected::Conflict
            });
        }

        // Commit only after canonical validation. A padding slot stays occupied
        // even though it has no Fabric representation.
        let newer = self.high.is_none_or(|high| sequence > high);
        if newer {
            self.high = Some(sequence);
            self.last_progress = Some(self.last_progress.map_or(arrival, |last| last.max(arrival)));
            if idle {
                self.resume_floor = sequence;
            }
            let oldest = self.oldest();
            while self
                .ledger
                .first_key_value()
                .is_some_and(|(&key, _)| key < oldest)
            {
                self.ledger.pop_first();
            }
        }
        if let Some(packet) = &packet {
            let raw = packet.header().timestamp;
            self.clock = match self.clock {
                Clock::Unanchored => Clock::Tracking(u64::from(raw)),
                Clock::Tracking(anchor) if newer => self
                    .extended_timestamp(raw)
                    .map_or(Clock::Ambiguous, |extended| {
                        Clock::Tracking(anchor.max(extended))
                    }),
                clock => clock,
            };
        }
        self.last_progress.get_or_insert(arrival);
        self.ledger.insert(
            sequence,
            Entry {
                admitted_at: arrival,
                media: packet.as_ref().map(|packet| CopyEvidence {
                    primary_presence: primary.then(|| presence(packet.extensions())),
                    additional_values: Vec::new(),
                }),
            },
        );
        Ok(packet.map_or(IdentityAdmission::Padding, |packet| {
            IdentityAdmission::Media {
                sequence,
                arrival,
                original: Some(packet.copy_payload()),
            }
        }))
    }
}

impl CopyEvidence {
    fn observe(
        &mut self,
        retained: &FabricPacket<Box<[u8]>>,
        copy: &FabricPacket<&[u8]>,
        primary: bool,
    ) -> bool {
        let original = retained.header();
        let header = copy.header();
        if original.profile != header.profile
            || original.timestamp != header.timestamp
            || original.marker != header.marker
            || retained.payload() != copy.payload()
        {
            return false;
        }
        let seen = presence(copy.extensions());
        if primary
            && self
                .primary_presence
                .is_some_and(|expected| expected != seen)
        {
            return false;
        }
        let mut novel = ArrayVec::<_, 8>::new();
        for ext in copy.extensions() {
            let known = retained
                .extensions()
                .find(|old| old.kind == ext.kind)
                .map(|old| old.value)
                .or_else(|| {
                    self.additional_values
                        .iter()
                        .find(|(kind, _)| *kind == ext.kind)
                        .map(|(_, value)| value.as_ref())
                });
            match known {
                Some(value) if value != ext.value => return false,
                Some(_) => {}
                None => novel.push(ext),
            }
        }
        self.additional_values.extend(
            novel
                .into_iter()
                .map(|ext| (ext.kind, Box::from(ext.value))),
        );
        if primary {
            self.primary_presence = Some(seen);
        }
        true
    }
}

fn presence<'a>(extensions: impl Iterator<Item = Extension<'a>>) -> u16 {
    extensions.fold(0, |mask, ext| mask | (1 << ext.kind as u8))
}

fn unwrap(low: u64, anchor: u64, cycle: u64) -> Result<u64, Rejected> {
    // Both callers use power-of-two RTP field widths.
    let mask = cycle.saturating_sub(1);
    let ahead = low.wrapping_sub(anchor & mask) & mask;
    let half = cycle / 2;
    match ahead.cmp(&half) {
        std::cmp::Ordering::Equal => Err(Rejected::Ambiguous),
        std::cmp::Ordering::Less => anchor.checked_add(ahead).ok_or(Rejected::Epoch),
        std::cmp::Ordering::Greater => anchor
            .checked_sub(cycle.saturating_sub(ahead))
            .ok_or(Rejected::PreOrigin),
    }
}

#[cfg(test)]
mod tests;
