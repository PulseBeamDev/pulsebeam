use ahash::{HashMap, HashMapExt};
use slotmap::SecondaryMap;
use str0m::channel::ChannelId;

use crate::entity::ParticipantId;
use crate::keys::{ReliableStreamKey, UnreliableStreamKey};
use crate::participant::reliable::ReliableChannels;
use crate::track::{DataTopicChannel, Topic};

pub(super) struct DataState {
    pub topic_channels: HashMap<ChannelId, DataTopicChannel>,
    pub published_channels: HashMap<Topic, ChannelId>,
    pub forwarding: SecondaryMap<UnreliableStreamKey, ChannelId>,
    pub reliable_forwarding: SecondaryMap<ReliableStreamKey, ChannelId>,
    pub published_streams: HashMap<ChannelId, UnreliableStreamKey>,
    pub reliable_published_streams: HashMap<ChannelId, ReliableStreamKey>,
    pub reliable_stream_topics: SecondaryMap<ReliableStreamKey, Topic>,
    pub reliable_subscribed_streams: HashMap<ChannelId, ReliableStreamKey>,
    pub subscribed_channels: HashMap<(Topic, Option<ParticipantId>), ChannelId>,
    pub reliable: ReliableChannels,
    pub pending_published_streams: HashMap<Topic, UnreliableStreamKey>,
    pub pending_reliable_streams: HashMap<Topic, ReliableStreamKey>,
}

impl DataState {
    pub(super) fn new() -> Self {
        Self {
            topic_channels: HashMap::new(),
            published_channels: HashMap::new(),
            forwarding: SecondaryMap::new(),
            reliable_forwarding: SecondaryMap::new(),
            published_streams: HashMap::new(),
            reliable_published_streams: HashMap::new(),
            reliable_stream_topics: SecondaryMap::new(),
            reliable_subscribed_streams: HashMap::new(),
            subscribed_channels: HashMap::new(),
            reliable: ReliableChannels::new(),
            pending_published_streams: HashMap::new(),
            pending_reliable_streams: HashMap::new(),
        }
    }
}
