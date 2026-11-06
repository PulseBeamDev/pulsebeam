use crate::entity::TrackId;
use crate::keys::TrackHandle;
use crate::media::ChannelId;
use ahash::{HashMap, HashMapExt};
use slotmap::SecondaryMap;

use crate::track::{
    DataLane, DataTopicChannel, DataTrackDirection, Topic, Track, TrackMeta, TrackSelector,
};

#[derive(thiserror::Error, Debug)]
pub enum DataOpenError {
    #[error("Duplicate data channel label for same direction: {0}")]
    DuplicateDataChannelLabel(DataTopicChannel),
    #[error(
        "Exceeded maximum data topic channels: only 64 channels (across all topics/scopes) allowed"
    )]
    TooManyDataTopicChannels,
}

pub(super) struct DataChannelState {
    pub(super) channel: DataTopicChannel,
    pub(super) publication: Option<TrackMeta>,
}

struct DataCandidate {
    id: TrackId,
    topic: Topic,
    lane: DataLane,
    channel: Option<ChannelId>,
}

impl DataCandidate {
    fn matches(&self, channel: &DataTopicChannel) -> bool {
        channel.direction == DataTrackDirection::Subscribe
            && channel.topic == self.topic
            && channel.lane == self.lane
    }
}

pub(super) struct DataState {
    channels: HashMap<ChannelId, DataChannelState>,
    sources: SecondaryMap<TrackHandle, ChannelId>,
    candidates: SecondaryMap<TrackHandle, DataCandidate>,
    next_publication: u64,
}

impl DataState {
    pub(super) fn new() -> Self {
        Self {
            channels: HashMap::new(),
            sources: SecondaryMap::new(),
            candidates: SecondaryMap::new(),
            next_publication: 0,
        }
    }

    pub(super) fn allocate_publication(
        &mut self,
        connection: crate::entity::ConnectionId,
        label: &str,
    ) -> Option<crate::entity::TrackId> {
        let incarnation = self.next_publication.checked_add(1)?;
        self.next_publication = incarnation;
        Some(connection.derive_track_id(
            crate::entity::TrackKind::Data,
            &format!("{label}/{incarnation}"),
        ))
    }

    pub(super) fn open(
        &mut self,
        cid: ChannelId,
        channel: DataTopicChannel,
        publication: Option<TrackMeta>,
    ) -> Result<(), DataOpenError> {
        if self.channels.len() >= crate::track::MAX_DATA_TOPIC_CHANNELS {
            return Err(DataOpenError::TooManyDataTopicChannels);
        }
        let duplicate = self.channels.iter().any(|(existing_id, state)| {
            let existing = &state.channel;
            *existing_id != cid
                && existing.direction == channel.direction
                && existing.topic == channel.topic
                && existing.lane == channel.lane
        });
        if duplicate {
            return Err(DataOpenError::DuplicateDataChannelLabel(channel));
        }
        debug_assert_eq!(
            publication.is_some(),
            channel.direction == DataTrackDirection::Publish
        );
        for candidate in self.candidates.values_mut() {
            if candidate.matches(&channel) {
                candidate.channel = Some(cid);
            }
        }
        self.channels.insert(
            cid,
            DataChannelState {
                channel,
                publication,
            },
        );
        Ok(())
    }

    pub(super) fn close(&mut self, cid: ChannelId) -> Option<DataChannelState> {
        let state = self.channels.remove(&cid)?;
        self.sources.retain(|_, channel| *channel != cid);
        for candidate in self.candidates.values_mut() {
            if candidate.channel == Some(cid) {
                candidate.channel = None;
            }
        }
        Some(state)
    }

    pub(super) fn bind_source(&mut self, track: TrackId, handle: TrackHandle) -> bool {
        let Some(channel) = self.channels.iter().find_map(|(channel, state)| {
            state
                .publication
                .as_ref()
                .filter(|meta| meta.id == track)
                .map(|_| *channel)
        }) else {
            return false;
        };
        self.sources
            .retain(|key, bound| *bound != channel || key == handle);
        self.sources.insert(handle, channel);
        true
    }

    pub(super) fn unpublish(&mut self, track: TrackId, handle: TrackHandle) {
        if let Some(channel) = self.sources.get(handle).copied()
            && self
                .publication(channel)
                .is_some_and(|meta| meta.id == track)
        {
            self.sources.remove(handle);
        }
    }

    pub(super) fn source(&self, handle: TrackHandle) -> Option<ChannelId> {
        self.sources.get(handle).copied()
    }

    pub(super) fn published_stream(&self, channel: ChannelId) -> Option<TrackHandle> {
        self.sources
            .iter()
            .find_map(|(handle, bound)| (*bound == channel).then_some(handle))
    }

    pub(super) fn add_candidate(&mut self, handle: TrackHandle, track: &Track) {
        let Track::Data(data) = track else {
            return;
        };
        let Some(topic) = &data.topic else {
            return;
        };
        let mut candidate = DataCandidate {
            id: track.id(),
            topic: topic.clone(),
            lane: data.lane,
            channel: None,
        };
        candidate.channel = self
            .channels
            .iter()
            .find_map(|(channel, state)| candidate.matches(&state.channel).then_some(*channel));
        self.candidates.insert(handle, candidate);
    }

    pub(super) fn remove_candidate(&mut self, handle: TrackHandle) {
        self.candidates.remove(handle);
    }

    pub(super) fn forwarding(&self, handle: TrackHandle) -> Option<ChannelId> {
        self.candidates.get(handle)?.channel
    }

    pub(super) fn subscribed_stream(
        &self,
        channel: ChannelId,
        track: TrackId,
    ) -> Option<TrackHandle> {
        self.candidates.iter().find_map(|(handle, candidate)| {
            (candidate.channel == Some(channel) && candidate.id == track).then_some(handle)
        })
    }

    pub(super) fn channel(&self, cid: ChannelId) -> Option<&DataTopicChannel> {
        self.channels.get(&cid).map(|state| &state.channel)
    }

    pub(super) fn accepts_publisher_frame(
        &self,
        cid: ChannelId,
        frame: &pulsebeam_proto::topic::TopicFrame,
    ) -> bool {
        let Some(state) = self.channels.get(&cid) else {
            return false;
        };
        let Some(publication) = state.publication.as_ref() else {
            return false;
        };
        state.channel.direction == DataTrackDirection::Publish
            && frame.data_track_id() == Some(publication.id.as_str().as_str())
            && frame.is_publisher_output()
            && (state.channel.lane == crate::track::DataLane::Ordered
                || matches!(
                    frame.body,
                    Some(pulsebeam_proto::topic::topic_frame::Body::Message(_))
                ))
    }

    pub(super) fn publication(&self, cid: ChannelId) -> Option<&TrackMeta> {
        self.channels.get(&cid)?.publication.as_ref()
    }

    pub(super) fn selector(channel: &DataTopicChannel) -> TrackSelector {
        TrackSelector::data_topic(
            None,
            crate::track::publication_label(channel.lane, &channel.topic),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        entity::{ConnectionId, ParticipantId, RoomExternalId, RoomId, TrackKind},
        id::ShardId,
        track::{DataLane, Topic},
    };

    #[test]
    fn subscriber_controls_cannot_target_another_channel_or_retired_track() {
        let connection = ConnectionId::new();
        let first = connection.derive_track_id(TrackKind::Data, "first");
        let second = connection.derive_track_id(TrackKind::Data, "second");
        let foreign = ConnectionId::new().derive_track_id(TrackKind::Data, "foreign");
        let mut handles = slotmap::SlotMap::<TrackHandle, ()>::with_key();
        let first_handle = handles.insert(());
        let second_handle = handles.insert(());
        let first_channel = ChannelId::from_value(0);
        let second_channel = ChannelId::from_value(2);
        let mut data = DataState::new();
        for (channel, label) in [(first_channel, "first"), (second_channel, "second")] {
            data.open(
                channel,
                DataTopicChannel {
                    direction: DataTrackDirection::Subscribe,
                    topic: Topic::for_test(label),
                    lane: DataLane::Ordered,
                },
                None,
            )
            .unwrap();
        }
        let make_track = |id, label: &str| {
            Track::data(
                TrackMeta {
                    room_id: RoomId::from_external(&RoomExternalId::new("data-routing").unwrap()),
                    shard_id: ShardId::new(0),
                    id,
                    origin: ParticipantId::new(),
                    connection_id: connection,
                    label: Some(label.into()),
                },
                Topic::for_test(label),
                DataLane::Ordered,
                None,
            )
        };
        data.add_candidate(first_handle, &make_track(first, "first"));
        data.add_candidate(second_handle, &make_track(second, "second"));
        assert_eq!(data.forwarding(first_handle), Some(first_channel));
        assert_eq!(
            data.subscribed_stream(first_channel, first),
            Some(first_handle)
        );
        assert_eq!(data.subscribed_stream(first_channel, second), None);
        assert_eq!(data.subscribed_stream(second_channel, first), None);
        assert_eq!(data.subscribed_stream(first_channel, foreign), None);
        data.close(first_channel);
        assert_eq!(data.subscribed_stream(first_channel, first), None);
        assert_eq!(data.forwarding(first_handle), None);
        let replacement = ChannelId::from_value(4);
        data.open(
            replacement,
            DataTopicChannel {
                direction: DataTrackDirection::Subscribe,
                topic: Topic::for_test("first"),
                lane: DataLane::Ordered,
            },
            None,
        )
        .unwrap();
        assert_eq!(data.forwarding(first_handle), Some(replacement));
        assert_eq!(data.subscribed_stream(first_channel, first), None);
        assert_eq!(
            data.subscribed_stream(replacement, first),
            Some(first_handle)
        );
        data.remove_candidate(second_handle);
        assert_eq!(data.subscribed_stream(second_channel, second), None);
    }

    #[test]
    fn publication_identity_is_fresh_on_reopen_and_exhaustion_is_explicit() {
        let connection = ConnectionId::new();
        let mut state = DataState::new();
        let first = state
            .allocate_publication(connection, "v2/ordered/chat")
            .unwrap();
        let second = state
            .allocate_publication(connection, "v2/ordered/chat")
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(first.kind(), TrackKind::Data);
        assert_ne!(
            second,
            state
                .allocate_publication(ConnectionId::new(), "v2/ordered/chat")
                .unwrap()
        );
        state.next_publication = u64::MAX;
        assert_eq!(
            state.allocate_publication(connection, "v2/ordered/chat"),
            None
        );
    }

    #[test]
    fn data_publication_closes_with_its_original_connection_metadata() {
        let cid = crate::media::ChannelId::from_value(0);
        let connection_id = ConnectionId::new();
        let topic = Topic::for_test("chat");
        let lane = DataLane::Latest;
        let label = crate::track::publication_label(lane, &topic);
        let meta = TrackMeta {
            room_id: RoomId::from_external(&RoomExternalId::new("data-lifecycle").unwrap()),
            shard_id: ShardId::new(0),
            id: connection_id.derive_track_id(TrackKind::Data, &label),
            origin: ParticipantId::new(),
            connection_id,
            label: Some(label),
        };
        let channel = DataTopicChannel {
            direction: DataTrackDirection::Publish,
            topic,
            lane,
        };

        let mut state = DataState::new();
        let mut handles = slotmap::SlotMap::<TrackHandle, ()>::with_key();
        let old = handles.insert(());
        let current = handles.insert(());
        assert!(!state.bind_source(meta.id, old));
        state
            .open(cid, channel.clone(), Some(meta.clone()))
            .unwrap();
        assert_eq!(state.published_stream(cid), None);
        assert!(state.bind_source(meta.id, old));
        assert_eq!(state.published_stream(cid), Some(old));
        assert!(state.bind_source(meta.id, current));
        state.unpublish(meta.id, old);
        assert_eq!(state.source(old), None);
        assert_eq!(state.published_stream(cid), Some(current));
        assert_eq!(state.channel(cid), Some(&channel));
        use pulsebeam_proto::topic::{
            TopicFrame, TopicFrontier, TopicMessage, TopicNack, topic_frame::Body,
        };
        let message = |id: crate::entity::TrackId| TopicFrame {
            body: Some(Body::Message(TopicMessage {
                data_track_id: id.as_str(),
                sequence: 0,
                payload: vec![1],
            })),
        };
        assert!(state.accepts_publisher_frame(cid, &message(meta.id)));
        assert!(!state.accepts_publisher_frame(
            cid,
            &message(connection_id.derive_track_id(TrackKind::Data, "another-publication"))
        ));
        assert!(!state.accepts_publisher_frame(
            cid,
            &TopicFrame {
                body: Some(Body::Frontier(TopicFrontier {
                    data_track_id: meta.id.as_str(),
                    next_sequence: 0
                }))
            }
        ));
        assert!(!state.accepts_publisher_frame(
            cid,
            &TopicFrame {
                body: Some(Body::Nack(TopicNack {
                    data_track_id: meta.id.as_str(),
                    from_sequence: 0
                }))
            }
        ));
        let closed = state.close(cid).unwrap();
        assert_eq!(state.published_stream(cid), None);
        assert_eq!(state.source(current), None);
        assert!(!state.bind_source(meta.id, old));
        assert_eq!(closed.channel, channel);
        assert_eq!(closed.publication, Some(meta.clone()));
        assert!(state.close(cid).is_none());
        assert!(!state.accepts_publisher_frame(cid, &message(meta.id)));
        let mut replacement = meta.clone();
        replacement.id = state
            .allocate_publication(connection_id, meta.label.as_deref().unwrap())
            .unwrap();
        state.open(cid, channel, Some(replacement.clone())).unwrap();
        assert!(!state.accepts_publisher_frame(cid, &message(meta.id)));
        assert!(state.accepts_publisher_frame(cid, &message(replacement.id)));
    }

    #[test]
    fn subscription_channel_has_no_publication_metadata() {
        let cid = crate::media::ChannelId::from_value(0);
        let channel = DataTopicChannel {
            direction: DataTrackDirection::Subscribe,
            topic: Topic::for_test("chat"),
            lane: DataLane::Ordered,
        };
        let mut state = DataState::new();
        state.open(cid, channel.clone(), None).unwrap();
        let closed = state.close(cid).unwrap();
        assert_eq!(closed.channel, channel);
        assert!(closed.publication.is_none());
    }
}
