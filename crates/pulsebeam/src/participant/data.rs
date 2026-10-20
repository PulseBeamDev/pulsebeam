use ahash::{HashMap, HashMapExt};
use str0m::channel::ChannelId;

use crate::track::{DataTopicChannel, DataTrackDirection, TrackMeta, TrackSelector};

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

pub(super) struct DataState {
    channels: HashMap<ChannelId, DataChannelState>,
    next_publication: u64,
}

impl DataState {
    pub(super) fn new() -> Self {
        Self {
            channels: HashMap::new(),
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
                && (channel.direction == DataTrackDirection::Publish
                    || existing.scope == channel.scope
                    || existing.scope.is_none()
                    || channel.scope.is_none())
        });
        if duplicate {
            return Err(DataOpenError::DuplicateDataChannelLabel(channel));
        }
        debug_assert_eq!(
            publication.is_some(),
            channel.direction == DataTrackDirection::Publish
        );
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
        self.channels.remove(&cid)
    }

    pub(super) fn channel(&self, cid: ChannelId) -> Option<&DataTopicChannel> {
        self.channels.get(&cid).map(|state| &state.channel)
    }

    pub(super) fn publications(&self) -> impl Iterator<Item = &TrackMeta> {
        self.channels
            .values()
            .filter_map(|state| state.publication.as_ref())
    }

    pub(super) fn channels_snapshot(&self) -> Vec<(ChannelId, DataTopicChannel)> {
        self.channels
            .iter()
            .map(|(cid, state)| (*cid, state.channel.clone()))
            .collect()
    }

    pub(super) fn selector(channel: &DataTopicChannel) -> TrackSelector {
        TrackSelector::data_topic(
            channel.scope,
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
    fn publication_identity_is_fresh_on_reopen_and_exhaustion_is_explicit() {
        let connection = ConnectionId::new();
        let mut state = DataState::new();
        let first = state
            .allocate_publication(connection, "v1/rel/chat")
            .unwrap();
        let second = state
            .allocate_publication(connection, "v1/rel/chat")
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(first.kind(), TrackKind::Data);
        assert_ne!(
            second,
            state
                .allocate_publication(ConnectionId::new(), "v1/rel/chat")
                .unwrap()
        );
        state.next_publication = u64::MAX;
        assert_eq!(state.allocate_publication(connection, "v1/rel/chat"), None);
    }

    #[test]
    fn data_publication_closes_with_its_original_connection_metadata() {
        let mut rtc = str0m::Rtc::new(std::time::Instant::now());
        let cid = rtc.direct_api().create_data_channel(Default::default());
        let connection_id = ConnectionId::new();
        let topic = Topic::for_test("chat");
        let lane = DataLane::Realtime;
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
            scope: None,
            lane,
        };

        let mut state = DataState::new();
        state
            .open(cid, channel.clone(), Some(meta.clone()))
            .unwrap();
        assert_eq!(state.channel(cid), Some(&channel));
        let closed = state.close(cid).unwrap();
        assert_eq!(closed.channel, channel);
        assert_eq!(closed.publication, Some(meta));
        assert!(state.close(cid).is_none());
    }

    #[test]
    fn subscription_channel_has_no_publication_metadata() {
        let mut rtc = str0m::Rtc::new(std::time::Instant::now());
        let cid = rtc.direct_api().create_data_channel(Default::default());
        let channel = DataTopicChannel {
            direction: DataTrackDirection::Subscribe,
            topic: Topic::for_test("chat"),
            scope: None,
            lane: DataLane::Reliable,
        };
        let mut state = DataState::new();
        state.open(cid, channel.clone(), None).unwrap();
        let closed = state.close(cid).unwrap();
        assert_eq!(closed.channel, channel);
        assert!(closed.publication.is_none());
    }
}
