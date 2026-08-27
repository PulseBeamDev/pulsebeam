use pulsebeam_rtc::ChannelId;
use slotmap::SecondaryMap;
use std::collections::VecDeque;

use crate::{
    keys::TrackKey,
    track::{DataTopicChannel, DataTrackDirection, Track},
};

pub struct DownstreamData {
    delivered: VecDeque<(TrackKey, usize)>,
    forwarding: SecondaryMap<TrackKey, ChannelId>,
}

impl DownstreamData {
    pub(crate) fn new() -> Self {
        Self {
            delivered: VecDeque::new(),
            forwarding: SecondaryMap::new(),
        }
    }

    pub(crate) fn add_candidate(
        &mut self,
        key: TrackKey,
        track: &Track,
        channels: &[(ChannelId, DataTopicChannel)],
    ) {
        let Track::Data(data) = track else {
            return;
        };
        let channel = channels.iter().find_map(|(cid, channel)| {
            (channel.direction == DataTrackDirection::Subscribe
                && channel.topic == data.topic
                && channel.lane == data.lane
                && channel
                    .scope
                    .is_none_or(|publisher| publisher == track.meta().origin))
            .then_some(*cid)
        });
        if let Some(channel) = channel {
            let previous = self.forwarding.insert(key, channel);
            debug_assert!(previous.is_none() || previous == Some(channel));
        }
    }

    pub(crate) fn remove_candidate(&mut self, key: TrackKey) {
        self.forwarding.remove(key);
    }

    pub(crate) fn forwarding(&self, key: TrackKey) -> Option<ChannelId> {
        self.forwarding.get(key).copied()
    }

    pub(crate) fn subscribed_stream(&self, cid: ChannelId) -> Option<TrackKey> {
        self.forwarding
            .iter()
            .find_map(|(key, channel)| (*channel == cid).then_some(key))
    }

    pub(crate) fn close(&mut self, cid: ChannelId) {
        self.forwarding.retain(|_, bound| *bound != cid);
    }

    pub(crate) fn record_delivery(&mut self, key: TrackKey, bytes: usize) {
        debug_assert!(bytes > 0);
        if self.delivered.len() == 32 {
            self.delivered.pop_front();
        }
        self.delivered.push_back((key, bytes));
    }
}
