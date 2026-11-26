use alloc::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    string::String,
    vec::Vec,
};
use pulsebeam_proto::state::{self as wire, validation};

use crate::{
    DesiredState, MediaSlot, SignalingError, TopicMode, TopicSubscriber, TrackOwner, TrackSelector,
    VideoSubscription,
    ffi::{ErrorCode, ErrorInfo},
    native_state::{NativeState, Resource},
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, uniffi::Enum)]
pub enum SelectorTrackKind {
    Audio,
    Video,
    Data,
}

impl SelectorTrackKind {
    fn wire(self) -> wire::TrackKind {
        match self {
            Self::Audio => wire::TrackKind::Audio,
            Self::Video => wire::TrackKind::Video,
            Self::Data => wire::TrackKind::Data,
        }
    }
}

/// `None` matches any value within the authenticated subscription authority.
#[derive(Clone, Debug, Default, Eq, PartialEq, uniffi::Record)]
pub struct SubscriptionSelector {
    pub project_id: Option<String>,
    pub room: Option<String>,
    pub participant_id: Option<String>,
    pub kind: Option<SelectorTrackKind>,
    pub track_id: Option<String>,
}

impl SubscriptionSelector {
    fn spec(self) -> wire::ResourceSpec {
        wire::ResourceSpec {
            settings: Some(wire::ResourceSettings {
                enabled: Some(true),
                receive: None,
            }),
            declaration: Some(wire::resource_spec::Declaration::SubscriptionSelector(
                wire::SubscriptionSelectorSpec {
                    project_id: self.project_id,
                    room: self.room,
                    participant_id: self.participant_id,
                    kind: self.kind.map(|kind| kind.wire() as i32),
                    track_id: self.track_id,
                },
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, uniffi::Enum)]
pub enum SelectorStatus {
    Pending,
    Complete,
    Overflow,
    Rejected,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, uniffi::Record)]
pub struct SelectorMatch {
    pub room: String,
    pub participant_id: String,
    pub kind: SelectorTrackKind,
    pub track_id: String,
}

impl SelectorMatch {
    fn from_wire(source: &wire::LogicalSourceAddress) -> Option<Self> {
        Some(Self {
            room: source.room.clone(),
            participant_id: source.participant_id.clone(),
            kind: match source.kind() {
                wire::TrackKind::Audio => SelectorTrackKind::Audio,
                wire::TrackKind::Video => SelectorTrackKind::Video,
                wire::TrackKind::Data => SelectorTrackKind::Data,
                wire::TrackKind::Unspecified => return None,
            },
            track_id: source.track_id.clone(),
        })
    }

    pub fn address(&self) -> TrackSelector {
        TrackSelector {
            room: self.room.clone(),
            participant_id: self.participant_id.clone(),
            track_id: self.track_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, uniffi::Record)]
pub struct SelectorState {
    pub status: SelectorStatus,
    pub matches: Vec<SelectorMatch>,
    pub error: Option<ErrorInfo>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, uniffi::Record)]
pub struct TrackFilter {
    #[uniffi(default = None)]
    pub participant_id: Option<String>,
    #[uniffi(default = None)]
    pub track_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmittedMedia {
    pub(super) owner: TrackOwner,
    pub(super) matched: SelectorMatch,
    pub(super) membership: u64,
    pub(super) target: crate::MediaObservationTarget,
}

impl AdmittedMedia {
    pub fn participant_id(&self) -> &str {
        &self.matched.participant_id
    }

    pub fn track_id(&self) -> &str {
        &self.matched.track_id
    }

    pub fn publication_id(&self) -> &str {
        &self.target.source
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, uniffi::Enum)]
pub enum CollectionStatus {
    Waiting,
    Live,
    Recovering,
    Limited,
    Rejected,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmittedMediaSnapshot {
    pub observation_revision: u64,
    pub status: CollectionStatus,
    pub tracks: Vec<AdmittedMedia>,
    pub error: Option<ErrorInfo>,
}

#[derive(Default)]
pub(super) struct Selectors {
    declarations: BTreeMap<TrackOwner, wire::ResourceSpec>,
    memberships: BTreeMap<TrackOwner, Memberships>,
}

#[derive(Default)]
struct Memberships {
    last: u64,
    by_match: BTreeMap<SelectorMatch, u64>,
}

impl Memberships {
    fn replace(&mut self, matches: impl Iterator<Item = SelectorMatch>) {
        let mut old = core::mem::take(&mut self.by_match);
        for source in matches {
            if self.by_match.contains_key(&source) {
                continue;
            }
            let key = if let Some(current) = old.remove(&source) {
                current
            } else if let Some(next) = self.last.checked_add(1) {
                self.last = next;
                next
            } else {
                // Fail closed rather than grant previously retired authority.
                continue;
            };
            self.by_match.insert(source, key);
        }
    }
}

impl Selectors {
    pub(super) fn insert(
        &mut self,
        owner: TrackOwner,
        selector: SubscriptionSelector,
    ) -> Result<(), ErrorInfo> {
        if self.declarations.len() >= validation::MAX_SELECTOR_DECLARATIONS {
            return Err(ErrorInfo::new(
                ErrorCode::CapacityExceeded,
                "too many subscription selectors",
            ));
        }
        let spec = selector.spec();
        validation::validate_spec(&spec).map_err(|_| {
            ErrorInfo::new(ErrorCode::InvalidArgument, "invalid subscription selector")
        })?;
        self.declarations.insert(owner, spec);
        Ok(())
    }

    pub(super) fn remove(&mut self, owner: TrackOwner) -> bool {
        self.memberships.remove(&owner);
        self.declarations.remove(&owner).is_some()
    }

    pub(super) fn observe_memberships(&mut self, owner: TrackOwner, native: &NativeState) {
        if !self.declarations.contains_key(&owner) {
            return;
        }
        self.memberships.entry(owner).or_default().replace(
            native
                .selector_snapshot(owner.id())
                .into_iter()
                .flat_map(|snapshot| &snapshot.matches)
                .filter_map(SelectorMatch::from_wire),
        );
    }

    pub(super) fn membership(&self, owner: TrackOwner, source: &SelectorMatch) -> Option<u64> {
        self.memberships.get(&owner)?.by_match.get(source).copied()
    }

    pub(super) fn state(
        &self,
        owner: TrackOwner,
        native: Option<&NativeState>,
    ) -> Option<SelectorState> {
        self.declarations.get(&owner)?;
        let mut state = SelectorState {
            status: SelectorStatus::Pending,
            matches: Vec::new(),
            error: None,
        };
        if let Some(native) = native {
            if let Some(snapshot) = native.selector_snapshot(owner.id()) {
                state.status = match snapshot.status() {
                    wire::SelectorMatchStatus::Complete => SelectorStatus::Complete,
                    wire::SelectorMatchStatus::Overflow => SelectorStatus::Overflow,
                    _ => SelectorStatus::Pending,
                };
                state
                    .matches
                    .extend(snapshot.matches.iter().filter_map(SelectorMatch::from_wire));
            } else if let Some(evaluation) =
                native.evaluation(&Resource::SubscriptionSelector(owner.id()))
            {
                if let Some(failure) = evaluation.rejection.as_ref() {
                    state.status = SelectorStatus::Rejected;
                    state.error = Some(super::publication::publication_failure(failure));
                } else if matches!(evaluation.value.as_ref(), Some(wire::confirmed_state::Value::Absent(absent)) if absent.reason() == wire::AbsenceReason::PermissionRevoked)
                {
                    state.status = SelectorStatus::Rejected;
                    state.error = Some(ErrorInfo::new(
                        ErrorCode::PermissionDenied,
                        "subscription selector permission revoked",
                    ));
                }
            }
        }
        Some(state)
    }

    pub(super) fn contains_match(
        &self,
        owner: TrackOwner,
        source: &SelectorMatch,
        native: &NativeState,
    ) -> bool {
        self.any_match(owner, native, |matched| {
            matched.room == source.room
                && matched.participant_id == source.participant_id
                && matched.track_id == source.track_id
                && matched.kind() == source.kind.wire()
        })
    }

    pub(super) fn contains_data_match(
        &self,
        owner: TrackOwner,
        native: &NativeState,
        subscriber: &TopicSubscriber,
    ) -> bool {
        self.any_match(owner, native, |matched| {
            matched.kind() == wire::TrackKind::Data
                && matched.room == subscriber.room
                && matched.participant_id == subscriber.participant_id
                && matched.track_id == subscriber.topic
        })
    }

    fn any_match(
        &self,
        owner: TrackOwner,
        native: &NativeState,
        predicate: impl FnMut(&wire::LogicalSourceAddress) -> bool,
    ) -> bool {
        self.declarations.contains_key(&owner)
            && native
                .selector_snapshot(owner.id())
                .is_some_and(|snapshot| snapshot.matches.iter().any(predicate))
    }

    pub(super) fn receiver_demand<'a>(
        &self,
        native: &NativeState,
        explicit: &'a DesiredState,
    ) -> Cow<'a, DesiredState> {
        let mut matches = self
            .declarations
            .keys()
            .filter_map(|owner| native.selector_snapshot(owner.id()))
            .flat_map(|snapshot| &snapshot.matches)
            .filter_map(SelectorMatch::from_wire)
            .peekable();
        let previous_video_slots = || {
            native.resources().filter_map(|resource| {
                let Resource::LogicalReception(MediaSlot::RemoteVideo(slot)) = resource else {
                    return None;
                };
                let Some(wire::resource_spec::Declaration::LogicalSubscription(logical)) =
                    native.target_spec(resource)?.declaration.as_ref()
                else {
                    return None;
                };
                Some((
                    SelectorMatch::from_wire(logical.source.as_ref()?)?.address(),
                    *slot,
                ))
            })
        };
        if matches.peek().is_none()
            && (explicit.video.is_empty()
                || !previous_video_slots().any(|(address, slot)| {
                    explicit.video.iter().any(|video| {
                        video.slot != slot && video.selector.as_ref() == Some(&address)
                    })
                }))
        {
            return Cow::Borrowed(explicit);
        }
        Cow::Owned(project_receivers(explicit, matches, previous_video_slots()))
    }

    pub(super) fn reconcile(&self, native: &mut NativeState) -> Result<(), SignalingError> {
        let removed: Vec<_> = native
            .resources()
            .filter_map(|resource| match resource {
                Resource::SubscriptionSelector(owner)
                    if !self.declarations.contains_key(&TrackOwner(*owner)) =>
                {
                    Some(resource.clone())
                }
                _ => None,
            })
            .collect();
        for resource in removed {
            native.stage(resource, None)?;
        }
        for (owner, spec) in &self.declarations {
            native.stage(
                Resource::SubscriptionSelector(owner.id()),
                Some(spec.clone()),
            )?;
        }
        Ok(())
    }
}

fn project_receivers(
    explicit: &DesiredState,
    matches: impl Iterator<Item = SelectorMatch>,
    previous_video_slots: impl Iterator<Item = (TrackSelector, u32)>,
) -> DesiredState {
    let mut projected = explicit.clone();
    projected.video.retain(|video| video.selector.is_none());
    let mut occupied: BTreeSet<_> = projected.video.iter().map(|video| video.slot).collect();
    let mut video: BTreeMap<_, _> = explicit
        .video
        .iter()
        .filter_map(|video| Some((video.selector.clone()?, (Some(video.slot), video.clone()))))
        .collect();
    let mut audio: BTreeSet<_> = explicit.audio.selected.iter().cloned().collect();
    let mut data: BTreeSet<_> = explicit.topics.subscribers.iter().cloned().collect();
    for source in matches {
        let address = source.address();
        match source.kind {
            SelectorTrackKind::Video => {
                video.entry(address.clone()).or_insert_with(|| {
                    (
                        None,
                        VideoSubscription {
                            decode_target: None,
                            slot: 0,
                            publication_id: String::new(),
                            selector: Some(address),
                            height: 0,
                            min_height: 0,
                            min_fps: 0,
                            priority: 0,
                        },
                    )
                });
            }
            SelectorTrackKind::Audio => {
                audio.insert(address);
            }
            SelectorTrackKind::Data => {
                // Offer delivery is immutable for a source lifetime. Exact data
                // reconciliation selects only the registration matching that offer.
                for mode in [TopicMode::Latest, TopicMode::Lossy, TopicMode::Ordered] {
                    data.insert(TopicSubscriber {
                        room: address.room.clone(),
                        participant_id: address.participant_id.clone(),
                        topic: address.track_id.clone(),
                        mode,
                    });
                }
            }
        }
    }
    // A slot is assigned to the logical source, not to the owner that first
    // required it. Reserve in-use slots before consulting exact owner preferences.
    for (address, slot) in previous_video_slots {
        if !occupied.contains(&slot)
            && let Some((_, mut subscription)) = video.remove(&address)
        {
            occupied.insert(slot);
            subscription.slot = slot;
            projected.video.push(subscription);
        }
    }
    let mut unassigned = Vec::new();
    for (preferred, mut subscription) in video.into_values() {
        if let Some(slot) = preferred.filter(|slot| occupied.insert(*slot)) {
            subscription.slot = slot;
            projected.video.push(subscription);
        } else {
            unassigned.push(subscription);
        }
    }
    for mut subscription in unassigned {
        let Some(slot) = (0..=u32::MAX).find(|slot| occupied.insert(*slot)) else {
            break;
        };
        subscription.slot = slot;
        projected.video.push(subscription);
    }
    projected
        .video
        .sort_by_key(|subscription| subscription.slot);
    projected.audio.selected = audio.into_iter().collect();
    projected.topics.subscribers = data.into_iter().collect();
    projected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_selectors_keep_one_stable_video_receiver_until_the_last_match_leaves() {
        let source = |room: &str, kind| SelectorMatch {
            room: room.into(),
            participant_id: "same-publisher".into(),
            kind,
            track_id: "same-label".into(),
        };
        let primary = source("stage", SelectorTrackKind::Video);
        let other = source("green-room", SelectorTrackKind::Video);
        let audio = source("stage", SelectorTrackKind::Audio);
        let data = source("stage", SelectorTrackKind::Data);
        let explicit = DesiredState::default();
        let demand = project_receivers(
            &explicit,
            [
                primary.clone(),
                primary.clone(),
                other.clone(),
                audio.clone(),
                data.clone(),
                data,
            ]
            .into_iter(),
            [(primary.address(), 0)].into_iter(),
        );
        assert_eq!(demand.video.len(), 2);
        assert_eq!(
            demand
                .video
                .iter()
                .find(|v| v.selector.as_ref() == Some(&primary.address()))
                .unwrap()
                .slot,
            0
        );
        assert_eq!(demand.audio.selected, [audio.address()]);
        assert_eq!(
            demand.topics.subscribers.len(),
            3,
            "three mode candidates for one data receiver"
        );
        let old = || {
            demand
                .video
                .iter()
                .map(|v| (v.selector.clone().unwrap(), v.slot))
        };
        let one_reason = project_receivers(&explicit, [primary, other.clone()].into_iter(), old());
        assert_eq!(one_reason.video, demand.video);
        let last_removed = project_receivers(&explicit, [other.clone()].into_iter(), old());
        assert_eq!(
            last_removed.video,
            [demand
                .video
                .iter()
                .find(|v| v.selector.as_ref() == Some(&other.address()))
                .unwrap()
                .clone()]
        );
    }

    #[test]
    fn exact_and_collection_video_share_the_established_slot_in_both_release_orders() {
        let camera = SelectorMatch {
            room: "stage".into(),
            participant_id: "alice".into(),
            kind: SelectorTrackKind::Video,
            track_id: "camera".into(),
        };
        let exact = DesiredState {
            video: alloc::vec![VideoSubscription {
                decode_target: None,
                slot: 1,
                publication_id: String::new(),
                selector: Some(camera.address()),
                height: 720,
                min_height: 360,
                min_fps: 15,
                priority: 100,
            }],
            ..Default::default()
        };
        let set_only = DesiredState::default();
        let slot_from = |demand: &DesiredState| {
            demand
                .video
                .iter()
                .map(|video| (video.selector.as_ref().unwrap().clone(), video.slot))
                .collect::<Vec<_>>()
        };
        let first_set = project_receivers(&set_only, [camera.clone()].into_iter(), [].into_iter());
        assert_eq!(first_set.video[0].slot, 0);
        for first in [&first_set, &exact] {
            let slots = slot_from(first);
            let overlap =
                project_receivers(&exact, [camera.clone()].into_iter(), slots.into_iter());
            assert_eq!(overlap.video.len(), 1);
            let mut expected = exact.video[0].clone();
            expected.slot = first.video[0].slot;
            assert_eq!(overlap.video, [expected.clone()]);
            let exact_remains =
                project_receivers(&exact, [].into_iter(), slot_from(&overlap).into_iter());
            assert_eq!(exact_remains.video, [expected]);
            let set_remains = project_receivers(
                &set_only,
                [camera.clone()].into_iter(),
                slot_from(&overlap).into_iter(),
            );
            assert_eq!(set_remains.video.len(), 1);
            assert_eq!(set_remains.video[0].slot, first.video[0].slot);
            let removed =
                project_receivers(&set_only, [].into_iter(), slot_from(&overlap).into_iter());
            assert!(removed.video.is_empty());
        }
    }

    #[test]
    fn collection_memberships_only_refresh_for_sources_that_leave() {
        let source = |participant: &str| SelectorMatch {
            room: "stage".into(),
            participant_id: participant.into(),
            kind: SelectorTrackKind::Video,
            track_id: "camera".into(),
        };
        let alice = source("alice");
        let bob = source("bob");
        let mut all = Memberships::default();
        let mut exact = Memberships::default();
        all.replace([alice.clone(), bob.clone()].into_iter());
        exact.replace([alice.clone()].into_iter());
        let alice_original = all.by_match[&alice];
        let bob_original = all.by_match[&bob];
        let overlapping = exact.by_match[&alice];
        all.replace([bob.clone()].into_iter());
        assert!(!all.by_match.contains_key(&alice));
        all.replace([bob.clone(), alice.clone()].into_iter());
        assert_ne!(all.by_match[&alice], alice_original);
        assert_eq!(all.by_match[&bob], bob_original);
        assert_eq!(exact.by_match[&alice], overlapping);
    }

    #[test]
    fn selector_desire_replays_in_a_new_connection_and_can_be_closed_offline() {
        let mut selectors = Selectors::default();
        let owner = TrackOwner(11);
        let invalid = SubscriptionSelector {
            room: Some("room*".into()),
            ..Default::default()
        };
        assert_eq!(
            selectors.insert(owner, invalid).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let selector = SubscriptionSelector {
            kind: Some(SelectorTrackKind::Video),
            ..Default::default()
        };
        selectors.insert(owner, selector.clone()).unwrap();
        let resource = Resource::SubscriptionSelector(owner.id());
        let new_connection = || NativeState::new(64 * 1024, None).unwrap();
        let mut first = new_connection();
        selectors.reconcile(&mut first).unwrap();
        assert_eq!(first.target_spec(&resource), Some(&selector.spec()));
        let mut second = new_connection();
        assert_eq!(
            selectors.state(owner, Some(&second)).unwrap().status,
            SelectorStatus::Pending
        );
        selectors.reconcile(&mut second).unwrap();
        assert_eq!(first.target_spec(&resource), second.target_spec(&resource));
        assert!(selectors.remove(owner));
        selectors.reconcile(&mut second).unwrap();
        assert!(second.target_spec(&resource).is_none());
        assert!(selectors.state(owner, None).is_none());
    }
}
