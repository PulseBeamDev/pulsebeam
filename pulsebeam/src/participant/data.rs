use ahash::{HashMap, HashMapExt};
use pulsebeam_rtc::ChannelId;

use crate::track::{DataTopicChannel, DataTrackDirection, TrackSelector};

#[derive(thiserror::Error, Debug)]
pub enum DataOpenError {
    #[error("Duplicate data channel label for same direction: {0}")]
    DuplicateDataChannelLabel(DataTopicChannel),
    #[error(
        "Exceeded maximum data topic channels: only 64 channels (across all topics/scopes) allowed"
    )]
    TooManyDataTopicChannels,
}

pub(super) struct DataState {
    channels: HashMap<ChannelId, DataTopicChannel>,
}

impl DataState {
    pub(super) fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    pub(super) fn open(
        &mut self,
        cid: ChannelId,
        channel: DataTopicChannel,
    ) -> Result<(), DataOpenError> {
        if self.channels.len() >= crate::track::MAX_DATA_TOPIC_CHANNELS {
            return Err(DataOpenError::TooManyDataTopicChannels);
        }
        let duplicate = self.channels.iter().any(|(existing_id, existing)| {
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
        self.channels.insert(cid, channel);
        Ok(())
    }

    pub(super) fn close(&mut self, cid: ChannelId) -> Option<DataTopicChannel> {
        let channel = self.channels.remove(&cid)?;
        Some(channel)
    }

    pub(super) fn channel(&self, cid: ChannelId) -> Option<&DataTopicChannel> {
        self.channels.get(&cid)
    }

    pub(super) fn channels_snapshot(&self) -> Vec<(ChannelId, DataTopicChannel)> {
        self.channels
            .iter()
            .map(|(cid, channel)| (*cid, channel.clone()))
            .collect()
    }

    pub(super) fn selector(channel: &DataTopicChannel) -> TrackSelector {
        TrackSelector::data_topic(
            channel.scope,
            crate::track::publication_label(channel.lane, &channel.topic),
        )
    }
}
