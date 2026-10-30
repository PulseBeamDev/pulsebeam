use crate::media::ChannelId;
use ahash::{HashMap, HashMapExt};

use crate::track::{DataTopicChannel, DataTrackDirection, Topic, TrackMeta, TrackSelector};

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
    pub(super) seed: Option<u64>,
}

pub(super) struct DataState {
    channels: HashMap<ChannelId, DataChannelState>,
    pending_seeds: HashMap<Topic, u64>,
    next_publication: u64,
}

impl DataState {
    pub(super) fn new() -> Self {
        Self {
            channels: HashMap::new(),
            pending_seeds: HashMap::new(),
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
    ) -> Result<Option<(TrackSelector, u64)>, DataOpenError> {
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
        let seed = if channel.direction == DataTrackDirection::Subscribe
            && channel.lane == crate::track::DataLane::Ordered
        {
            self.pending_seeds.remove(&channel.topic)
        } else {
            None
        };
        let seeded = seed.map(|token| (Self::selector(&channel), token));
        self.channels.insert(
            cid,
            DataChannelState {
                channel,
                publication,
                seed,
            },
        );
        Ok(seeded)
    }

    pub(super) fn seed(
        &mut self,
        request: &pulsebeam_proto::signaling_v1::SubscriptionSeed,
    ) -> Result<Option<TrackSelector>, ()> {
        if request.seed_id == 0
            || request.mode != pulsebeam_proto::signaling_v1::TopicMode::Ordered as i32
        {
            return Err(());
        }
        let topic = Topic::parse(&request.topic).map_err(|_| ())?;
        if self
            .channels
            .values()
            .any(|state| state.seed == Some(request.seed_id) && state.channel.topic != topic)
            || self
                .pending_seeds
                .iter()
                .any(|(other, token)| *token == request.seed_id && *other != topic)
        {
            return Err(());
        }
        let state = self.channels.values_mut().find(|state| {
            state.channel.direction == DataTrackDirection::Subscribe
                && state.channel.lane == crate::track::DataLane::Ordered
                && state.channel.topic.as_ref() == request.topic
        });
        let Some(state) = state else {
            if let Some(token) = self.pending_seeds.get(&topic) {
                return if *token == request.seed_id {
                    Ok(None)
                } else {
                    Err(())
                };
            }
            if self.pending_seeds.len() >= crate::track::MAX_DATA_TOPIC_CHANNELS {
                return Err(());
            }
            // DCEP OPEN and signaling use different SCTP streams. A locally open
            // subscription can send its seed before its DCEP reaches this endpoint.
            self.pending_seeds.insert(topic, request.seed_id);
            return Ok(None);
        };
        if let Some(token) = state.seed {
            return if token == request.seed_id {
                Ok(None)
            } else {
                Err(())
            };
        }
        state.seed = Some(request.seed_id);
        Ok(Some(Self::selector(&state.channel)))
    }

    pub(super) fn has_seed(&self, token: u64) -> bool {
        self.channels
            .values()
            .any(|state| state.seed == Some(token))
    }

    pub(super) fn close(&mut self, cid: ChannelId) -> Option<DataChannelState> {
        self.channels.remove(&cid)
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
        assert!(
            state
                .open(cid, channel.clone(), Some(meta.clone()))
                .unwrap()
                .is_none()
        );
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
        assert_eq!(closed.channel, channel);
        assert_eq!(closed.publication, Some(meta.clone()));
        assert!(state.close(cid).is_none());
        assert!(!state.accepts_publisher_frame(cid, &message(meta.id)));
        let mut replacement = meta.clone();
        replacement.id = state
            .allocate_publication(connection_id, meta.label.as_deref().unwrap())
            .unwrap();
        assert!(
            state
                .open(cid, channel, Some(replacement.clone()))
                .unwrap()
                .is_none()
        );
        assert!(!state.accepts_publisher_frame(cid, &message(meta.id)));
        assert!(state.accepts_publisher_frame(cid, &message(replacement.id)));
    }

    #[test]
    fn seed_waits_for_dcep_and_tokens_are_scoped_to_live_ordered_subscriptions() {
        use pulsebeam_proto::signaling_v1::{SubscriptionSeed, TopicMode};
        let mut state = DataState::new();
        let cid = ChannelId::from_value(1);
        let channel = DataTopicChannel {
            direction: DataTrackDirection::Subscribe,
            topic: Topic::for_test("events"),
            lane: DataLane::Ordered,
        };
        let mut request = SubscriptionSeed {
            seed_id: 7,
            topic: "events".into(),
            mode: TopicMode::Ordered as i32,
        };
        assert!(state.seed(&request).unwrap().is_none());
        assert!(state.seed(&request).unwrap().is_none());
        assert!(!state.has_seed(7));
        let (selector, token) = state.open(cid, channel.clone(), None).unwrap().unwrap();
        assert_eq!(selector, DataState::selector(&channel));
        assert_eq!(token, 7);
        assert!(state.has_seed(7));
        assert!(
            state.seed(&request).unwrap().is_none(),
            "duplicate seed is idempotent"
        );
        request.seed_id = 8;
        assert!(state.seed(&request).is_err());
        assert!(state.has_seed(7));
        state.close(cid);
        assert!(!state.has_seed(7));
        assert!(state.open(cid, channel, None).unwrap().is_none());
        assert!(state.seed(&request).unwrap().is_some());
        request.seed_id = 0;
        assert!(state.seed(&request).is_err());
    }

    #[test]
    fn pending_seeds_validate_identity_and_bound_unopened_subscriptions() {
        use pulsebeam_proto::signaling_v1::{SubscriptionSeed, TopicMode};
        let mut state = DataState::new();
        let request = |seed_id, topic: String| SubscriptionSeed {
            seed_id,
            topic,
            mode: TopicMode::Ordered as i32,
        };
        for topic in ["".to_string(), "bad/topic".to_string(), "x".repeat(86)] {
            assert!(state.seed(&request(1, topic)).is_err());
        }
        assert!(state.seed(&request(0, "events".into())).is_err());
        let mut wrong_mode = request(1, "events".into());
        wrong_mode.mode = TopicMode::Latest as i32;
        assert!(state.seed(&wrong_mode).is_err());
        for index in 0..crate::track::MAX_DATA_TOPIC_CHANNELS {
            let token = u64::try_from(index).unwrap().saturating_add(1);
            assert!(
                state
                    .seed(&request(token, format!("events-{index}")))
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            state
                .seed(&request(1, "events-0".into()))
                .unwrap()
                .is_none()
        );
        assert!(state.seed(&request(1, "different".into())).is_err());
        assert!(state.seed(&request(u64::MAX, "events-0".into())).is_err());
        assert!(state.seed(&request(u64::MAX, "extra".into())).is_err());
        let channel = DataTopicChannel {
            direction: DataTrackDirection::Subscribe,
            topic: Topic::for_test("events-0"),
            lane: DataLane::Ordered,
        };
        let (_, token) = state
            .open(ChannelId::from_value(1), channel, None)
            .unwrap()
            .unwrap();
        assert_eq!(token, 1);
        assert!(state.seed(&request(1, "different".into())).is_err());
        assert!(
            state
                .seed(&request(u64::MAX, "extra".into()))
                .unwrap()
                .is_none()
        );
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
        assert!(state.open(cid, channel.clone(), None).unwrap().is_none());
        let closed = state.close(cid).unwrap();
        assert_eq!(closed.channel, channel);
        assert!(closed.publication.is_none());
    }
}
