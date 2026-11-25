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

#[derive(Clone, Debug, Eq, PartialEq, uniffi::Record)]
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

#[derive(Default)]
pub(super) struct Selectors(BTreeMap<TrackOwner, wire::ResourceSpec>);

impl Selectors {
    pub(super) fn insert(
        &mut self,
        owner: TrackOwner,
        selector: SubscriptionSelector,
    ) -> Result<(), ErrorInfo> {
        if self.0.len() >= validation::MAX_SELECTOR_DECLARATIONS {
            return Err(ErrorInfo::new(
                ErrorCode::CapacityExceeded,
                "too many subscription selectors",
            ));
        }
        let spec = selector.spec();
        validation::validate_spec(&spec).map_err(|_| {
            ErrorInfo::new(ErrorCode::InvalidArgument, "invalid subscription selector")
        })?;
        self.0.insert(owner, spec);
        Ok(())
    }

    pub(super) fn remove(&mut self, owner: TrackOwner) -> bool {
        self.0.remove(&owner).is_some()
    }

    pub(super) fn state(
        &self,
        owner: TrackOwner,
        native: Option<&NativeState>,
    ) -> Option<SelectorState> {
        self.0.get(&owner)?;
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
        self.0.contains_key(&owner)
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
            .0
            .keys()
            .filter_map(|owner| native.selector_snapshot(owner.id()))
            .flat_map(|snapshot| &snapshot.matches)
            .filter_map(SelectorMatch::from_wire)
            .peekable();
        if matches.peek().is_none() {
            return Cow::Borrowed(explicit);
        }
        Cow::Owned(project_receivers(
            explicit,
            matches,
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
            }),
        ))
    }

    pub(super) fn reconcile(&self, native: &mut NativeState) -> Result<(), SignalingError> {
        let removed: Vec<_> = native
            .resources()
            .filter_map(|resource| match resource {
                Resource::SubscriptionSelector(owner)
                    if !self.0.contains_key(&TrackOwner(*owner)) =>
                {
                    Some(resource.clone())
                }
                _ => None,
            })
            .collect();
        for resource in removed {
            native.stage(resource, None)?;
        }
        for (owner, spec) in &self.0 {
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
    let mut occupied: BTreeSet<_> = explicit.video.iter().map(|video| video.slot).collect();
    let explicit_video: BTreeSet<_> = explicit
        .video
        .iter()
        .filter_map(|video| video.selector.clone())
        .collect();
    let mut video = BTreeSet::new();
    let mut audio: BTreeSet<_> = explicit.audio.selected.iter().cloned().collect();
    let mut data: BTreeSet<_> = explicit.topics.subscribers.iter().cloned().collect();
    for source in matches {
        let address = source.address();
        match source.kind {
            SelectorTrackKind::Video if !explicit_video.contains(&address) => {
                video.insert(address);
            }
            SelectorTrackKind::Video => {}
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
    let mut reserved = BTreeMap::new();
    for (address, slot) in previous_video_slots {
        if video.contains(&address) && !reserved.contains_key(&address) && occupied.insert(slot) {
            reserved.insert(address, slot);
        }
    }
    for address in video {
        let slot = if let Some(slot) = reserved.remove(&address) {
            slot
        } else {
            let Some(slot) = (0..=u32::MAX).find(|slot| occupied.insert(*slot)) else {
                break;
            };
            slot
        };
        projected.video.push(VideoSubscription {
            decode_target: None,
            slot,
            publication_id: String::new(),
            selector: Some(address),
            height: 0,
            min_height: 0,
            min_fps: 0,
            priority: 0,
        });
    }
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
