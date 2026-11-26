use agent_core::{Agent, ffi};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResourceKind {
    Video,
    Audio,
    DataPublication,
    DataSubscription,
    MediaCollection,
}

#[derive(Clone, Debug)]
pub(crate) enum ResourceState {
    Video(ffi::VideoSubscriptionState),
    Audio(ffi::AudioSubscriptionState),
    DataPublication(ffi::DataPublicationState),
    DataSubscription(ffi::DataSubscriptionState),
    MediaCollection(agent_core::AdmittedMediaSnapshot),
}

impl ResourceKind {
    pub(crate) fn state(self, agent: &Agent, owner: u64) -> Option<ResourceState> {
        match self {
            Self::Video => agent
                .video_subscription_state(owner)
                .ok()
                .map(ResourceState::Video),
            Self::Audio => agent
                .audio_subscription_state(owner)
                .ok()
                .map(ResourceState::Audio),
            Self::DataPublication => agent
                .data_publication_state(owner)
                .ok()
                .map(ResourceState::DataPublication),
            Self::DataSubscription => agent
                .data_subscription_state(owner)
                .ok()
                .map(ResourceState::DataSubscription),
            Self::MediaCollection => agent
                .media_collection_state(owner)
                .ok()
                .map(ResourceState::MediaCollection),
        }
    }
}

impl ResourceState {
    pub(crate) fn mark_closed(&mut self) {
        match self {
            Self::Video(state) => state.closed = true,
            Self::Audio(state) => state.closed = true,
            Self::DataPublication(state) => state.closed = true,
            Self::DataSubscription(state) => state.closed = true,
            Self::MediaCollection(state) => {
                state.status = agent_core::CollectionStatus::Closed;
                state.tracks.clear();
            }
        }
    }
}

pub(crate) type ResourceNotification = Box<dyn FnOnce()>;

#[cfg(test)]
mod tests;
