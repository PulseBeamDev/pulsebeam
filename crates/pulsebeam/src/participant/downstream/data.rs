use crate::media::ChannelId;
use slotmap::SecondaryMap;
use std::collections::VecDeque;

use crate::{
    keys::TrackHandle,
    track::{DataTopicChannel, DataTrackDirection, Track},
};

pub struct DownstreamData {
    delivered: VecDeque<(TrackHandle, usize)>,
    forwarding: SecondaryMap<TrackHandle, ChannelId>,
    publications: SecondaryMap<TrackHandle, crate::track::TrackMeta>,
}

impl DownstreamData {
    pub(crate) fn new() -> Self {
        Self {
            delivered: VecDeque::new(),
            forwarding: SecondaryMap::new(),
            publications: SecondaryMap::new(),
        }
    }

    pub(crate) fn add_candidate(
        &mut self,
        key: TrackHandle,
        track: &Track,
        channels: &[(ChannelId, DataTopicChannel)],
    ) {
        let Track::Data(data) = track else {
            return;
        };
        self.publications.insert(key, data.meta.clone());
        let channel = channels.iter().find_map(|(cid, channel)| {
            (channel.direction == DataTrackDirection::Subscribe
                && channel.topic == data.topic
                && channel.lane == data.lane)
                .then_some(*cid)
        });
        if let Some(channel) = channel {
            let previous = self.forwarding.insert(key, channel);
            debug_assert!(previous.is_none() || previous == Some(channel));
        }
    }

    pub(crate) fn remove_candidate(&mut self, key: TrackHandle) {
        self.forwarding.remove(key);
        self.publications.remove(key);
    }

    pub(crate) fn publications(&self) -> impl Iterator<Item = &crate::track::TrackMeta> {
        self.publications.values()
    }

    pub(crate) fn forwarding(&self, key: TrackHandle) -> Option<ChannelId> {
        self.forwarding.get(key).copied()
    }

    pub(crate) fn subscribed_stream(
        &self,
        cid: ChannelId,
        track_id: &crate::entity::TrackId,
        tracks: &impl Fn(TrackHandle) -> Option<crate::entity::TrackId>,
    ) -> Option<TrackHandle> {
        self.forwarding.iter().find_map(|(key, channel)| {
            (*channel == cid && tracks(key) == Some(*track_id)).then_some(key)
        })
    }

    pub(crate) fn close(&mut self, cid: ChannelId) {
        self.forwarding.retain(|_, bound| *bound != cid);
    }

    pub(crate) fn record_delivery(&mut self, key: TrackHandle, bytes: usize) {
        debug_assert!(bytes > 0);
        if self.delivered.len() == 32 {
            self.delivered.pop_front();
        }
        self.delivered.push_back((key, bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{ConnectionId, TrackKind};
    use slotmap::SlotMap;

    #[test]
    fn subscriber_controls_cannot_target_another_channel_or_retired_track() {
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let connection = ConnectionId::new();
        let first = connection.derive_track_id(TrackKind::Data, "first");
        let second = connection.derive_track_id(TrackKind::Data, "second");
        let foreign = ConnectionId::new().derive_track_id(TrackKind::Data, "foreign");
        let mut tracks = SlotMap::<TrackHandle, _>::with_key();
        let first_key = tracks.insert(first);
        let second_key = tracks.insert(second);
        let first_channel = ChannelId::from_value(0);
        let second_channel = ChannelId::from_value(2);
        let mut data = DownstreamData::new();
        data.forwarding.insert(first_key, first_channel);
        data.forwarding.insert(second_key, second_channel);
        let lookup = |key| tracks.get(key).copied();
        assert_eq!(
            data.subscribed_stream(first_channel, &first, &lookup),
            Some(first_key)
        );
        assert_eq!(
            data.subscribed_stream(first_channel, &second, &lookup),
            None
        );
        assert_eq!(
            data.subscribed_stream(second_channel, &first, &lookup),
            None
        );
        assert_eq!(
            data.subscribed_stream(first_channel, &foreign, &lookup),
            None
        );
        data.close(first_channel);
        assert_eq!(data.subscribed_stream(first_channel, &first, &lookup), None);
        data.remove_candidate(second_key);
        assert_eq!(
            data.subscribed_stream(second_channel, &second, &lookup),
            None
        );
    }
}
