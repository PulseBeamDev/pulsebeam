use super::*;

macro_rules! media_collection {
    ($open:ident, $kind:ident, $reference:ident, $collection:ident, $state:ident, $updates:ident, $entry:ident, $track:ident, $frames:ident) => {
        #[uniffi::export]
        impl RoomRef {
            #[uniffi::method(default(filter = None))]
            pub fn $open(&self, filter: Option<agent_core::TrackFilter>) -> Arc<$reference> {
                Arc::new($reference {
                    room: self.runtime.clone(),
                    filter: filter.unwrap_or_default(),
                })
            }
        }

        #[derive(uniffi::Object)]
        pub struct $reference {
            room: crate::RoomRef,
            filter: agent_core::TrackFilter,
        }

        #[uniffi::export]
        impl $reference {
            pub fn subscribe(&self) -> Result<Arc<$collection>, NativeError> {
                Ok(Arc::new($collection {
                    runtime: self
                        .room
                        .media_collection(agent_core::SelectorTrackKind::$kind, self.filter.clone())
                        .map_err(operation_error)?,
                    room: self.room.clone(),
                }))
            }
        }

        #[derive(uniffi::Object)]
        pub struct $collection {
            room: crate::RoomRef,
            runtime: crate::native_actor::MediaCollection,
        }

        #[uniffi::export]
        impl $collection {
            pub fn state(&self) -> $state {
                $state::new(self.runtime.observer.state(), &self.room)
            }

            pub fn updates(&self) -> Result<Arc<$updates>, NativeError> {
                Ok(Arc::new($updates {
                    room: self.room.clone(),
                    runtime: self
                        .runtime
                        .observer
                        .updates()
                        .map_err(resource_state::observer_limit)?,
                }))
            }

            pub fn close(&self) {
                self.runtime.close();
            }
        }

        #[derive(uniffi::Record)]
        pub struct $state {
            pub observation_revision: u64,
            pub status: agent_core::CollectionStatus,
            pub tracks: Vec<$entry>,
            pub error: Option<core_ffi::ErrorInfo>,
        }

        #[derive(uniffi::Record)]
        pub struct $entry {
            pub participant_id: String,
            pub track_id: String,
            pub publication_id: String,
            pub track: Arc<$track>,
        }

        impl $state {
            fn new(state: agent_core::AdmittedMediaSnapshot, room: &crate::RoomRef) -> Self {
                Self {
                    observation_revision: state.observation_revision,
                    status: state.status,
                    tracks: state
                        .tracks
                        .into_iter()
                        .map(|admission| $entry {
                            participant_id: admission.participant_id().into(),
                            track_id: admission.track_id().into(),
                            publication_id: admission.publication_id().into(),
                            track: Arc::new($track {
                                room: room.clone(),
                                admission,
                            }),
                        })
                        .collect(),
                    error: state.error,
                }
            }
        }

        #[derive(uniffi::Object)]
        pub struct $track {
            room: crate::RoomRef,
            admission: agent_core::AdmittedMedia,
        }

        #[uniffi::export]
        impl $track {
            pub fn frames(&self) -> Result<Arc<$frames>, NativeError> {
                Ok(Arc::new($frames::admitted(
                    self.room.clone(),
                    self.admission.clone(),
                )?))
            }
        }

        #[derive(uniffi::Object)]
        pub struct $updates {
            room: crate::RoomRef,
            runtime: agent_core::observer::MediaCollectionUpdates,
        }

        #[uniffi::export(async_runtime = "tokio")]
        impl $updates {
            pub async fn next(&self) -> Result<Option<$state>, NativeError> {
                let next = self.runtime.next().map_err(|_| NativeError::Operation {
                    info: core_ffi::ErrorInfo::new(
                        core_ffi::ErrorCode::InvalidArgument,
                        "Collection updates already have a pending read",
                    ),
                })?;
                Ok(next.await.map(|state| $state::new(state, &self.room)))
            }

            pub fn close(&self) {
                self.runtime.close();
            }
        }
    };
}

media_collection!(
    video,
    Video,
    VideoCollectionRef,
    VideoCollection,
    VideoCollectionState,
    VideoCollectionUpdates,
    VideoCollectionEntry,
    VideoTrack,
    VideoFrames
);
media_collection!(
    audio,
    Audio,
    AudioCollectionRef,
    AudioCollection,
    AudioCollectionState,
    AudioCollectionUpdates,
    AudioCollectionEntry,
    AudioTrack,
    AudioFrames
);
