use ahash::{HashMap, HashMapExt};
use slotmap::SecondaryMap;
use str0m::channel::ChannelId;

use crate::entity::ParticipantId;
use crate::keys::TrackKey;
use crate::participant::reliable::ReliableChannels;
use crate::track::{DataLane, DataTopicChannel, Topic};

#[derive(Clone, Copy)]
pub(super) struct DataForwarding {
    pub lane: DataLane,
    pub channel: ChannelId,
}

pub(super) struct DataState {
    pub topic_channels: HashMap<ChannelId, DataTopicChannel>,
    pub published_channels: HashMap<Topic, ChannelId>,
    pub forwarding: SecondaryMap<TrackKey, DataForwarding>,
    pub published_streams: HashMap<ChannelId, TrackKey>,
    pub reliable_published_streams: HashMap<ChannelId, TrackKey>,
    pub reliable_stream_topics: SecondaryMap<TrackKey, Topic>,
    pub subscribed_channels: HashMap<(Topic, Option<ParticipantId>), ChannelId>,
    pub reliable: ReliableChannels,
    pub reliable_subscribed_streams: HashMap<ChannelId, TrackKey>,
    pub pending_published_streams: HashMap<Topic, TrackKey>,
    pub pending_reliable_streams: HashMap<Topic, TrackKey>,
}

impl DataState {
    pub(super) fn new() -> Self {
        Self {
            topic_channels: HashMap::new(),
            published_channels: HashMap::new(),
            forwarding: SecondaryMap::new(),
            published_streams: HashMap::new(),
            reliable_published_streams: HashMap::new(),
            reliable_stream_topics: SecondaryMap::new(),
            subscribed_channels: HashMap::new(),
            reliable: ReliableChannels::new(),
            reliable_subscribed_streams: HashMap::new(),
            pending_published_streams: HashMap::new(),
            pending_reliable_streams: HashMap::new(),
        }
    }
}
