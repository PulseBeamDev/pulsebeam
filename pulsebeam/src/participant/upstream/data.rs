use crate::participant::data::ChannelId;
use ahash::{HashMap, HashMapExt};
use slotmap::SecondaryMap;

use crate::{entity::TrackId, keys::TrackKey, track::Track};

pub(crate) struct UpstreamData {
    sources: SecondaryMap<TrackKey, ChannelId>,
    published: HashMap<TrackId, ChannelId>,
    pending_sources: HashMap<TrackId, TrackKey>,
}

impl UpstreamData {
    pub(super) fn new() -> Self {
        Self {
            sources: SecondaryMap::new(),
            published: HashMap::new(),
            pending_sources: HashMap::new(),
        }
    }

    pub(crate) fn bind_source(&mut self, track_id: TrackId, key: TrackKey) {
        if let Some(channel) = self.published.get(&track_id).copied() {
            let previous = self.sources.insert(key, channel);
            debug_assert!(previous.is_none() || previous == Some(channel));
        } else {
            self.pending_sources.insert(track_id, key);
        }
    }

    pub(crate) fn publish(&mut self, cid: ChannelId, track: &Track) {
        let previous = self.published.insert(track.id(), cid);
        debug_assert!(previous.is_none() || previous == Some(cid));
        if let Some(key) = self.pending_sources.remove(&track.id()) {
            let previous = self.sources.insert(key, cid);
            debug_assert!(previous.is_none() || previous == Some(cid));
        }
    }

    pub(crate) fn unpublish(&mut self, track_id: TrackId) {
        self.published.remove(&track_id);
        self.pending_sources.remove(&track_id);
    }

    pub(crate) fn close(&mut self, cid: ChannelId) {
        self.sources.retain(|_, bound| *bound != cid);
        self.published.retain(|_, bound| *bound != cid);
    }

    pub(crate) fn source(&self, key: TrackKey) -> Option<ChannelId> {
        self.sources.get(key).copied()
    }

    pub(crate) fn published_stream(&self, cid: ChannelId) -> Option<TrackKey> {
        self.sources
            .iter()
            .find_map(|(key, channel)| (*channel == cid).then_some(key))
    }
}
