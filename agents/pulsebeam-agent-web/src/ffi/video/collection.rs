use super::*;

#[cfg(target_arch = "wasm32")]
struct CollectionOwner {
    room: Arc<RoomRef>,
    owner: u64,
    observer: std::rc::Rc<agent_core::observer::MediaCollectionObserver>,
}

#[cfg(target_arch = "wasm32")]
impl CollectionOwner {
    fn close(&self) {
        if self.observer.state().status != agent_core::CollectionStatus::Closed {
            let _ = self.room.agent.runtime.release_media_owner(self.owner);
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for CollectionOwner {
    fn drop(&mut self) {
        self.close();
    }
}

macro_rules! media_collection {
    ($open:ident, $kind:ident, $reference:ident, $collection:ident, $state:ident, $updates:ident, $entry:ident, $track:ident, $output:ident) => {
        #[uniffi::export]
        impl RoomRef {
            #[uniffi::method(default(filter = None))]
            pub fn $open(
                self: Arc<Self>,
                filter: Option<agent_core::TrackFilter>,
            ) -> Arc<$reference> {
                Arc::new($reference {
                    room: self,
                    filter: filter.unwrap_or_default(),
                })
            }
        }

        #[derive(uniffi::Object)]
        pub struct $reference {
            room: Arc<RoomRef>,
            filter: agent_core::TrackFilter,
        }

        #[uniffi::export]
        impl $reference {
            pub fn same_selection(&self, other: Arc<Self>) -> bool {
                Arc::ptr_eq(&self.room.agent, &other.room.agent)
                    && self.room.room == other.room.room
                    && self.filter == other.filter
            }

            pub fn subscribe(&self) -> Result<Arc<$collection>, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    let (owner, observer) = self.room.agent.runtime.claim_media_collection(
                        self.room.room.clone(),
                        agent_core::SelectorTrackKind::$kind,
                        self.filter.clone(),
                    )?;
                    Ok(Arc::new($collection {
                        runtime: CollectionOwner {
                            room: self.room.clone(),
                            owner,
                            observer,
                        },
                    }))
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let _ = (&self.room, &self.filter);
                    Err(crate::ffi::browser_required())
                }
            }
        }

        #[derive(uniffi::Object)]
        pub struct $collection {
            #[cfg(target_arch = "wasm32")]
            runtime: CollectionOwner,
        }

        #[uniffi::export]
        impl $collection {
            pub fn state(&self) -> Result<$state, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    Ok($state::new(
                        self.runtime.observer.state(),
                        &self.runtime.room,
                    ))
                }
                #[cfg(not(target_arch = "wasm32"))]
                Err(crate::ffi::browser_required())
            }

            pub fn updates(&self) -> Result<Arc<$updates>, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    Ok(Arc::new($updates {
                        room: self.runtime.room.clone(),
                        cursor: self.runtime.observer.updates().map_err(|_| {
                            ErrorInfo::new(
                                ErrorCode::CapacityExceeded,
                                "Too many collection update cursors",
                            )
                        })?,
                    }))
                }
                #[cfg(not(target_arch = "wasm32"))]
                Err(crate::ffi::browser_required())
            }

            pub async fn close(&self) {
                #[cfg(target_arch = "wasm32")]
                self.runtime.close();
            }
        }

        #[derive(uniffi::Record)]
        pub struct $state {
            pub observation_revision: u64,
            pub status: agent_core::CollectionStatus,
            pub tracks: Vec<$entry>,
            pub error: Option<ErrorInfo>,
        }

        #[derive(uniffi::Record)]
        pub struct $entry {
            pub participant_id: String,
            pub track_id: String,
            pub publication_id: String,
            pub track: Arc<$track>,
        }

        #[cfg(target_arch = "wasm32")]
        impl $state {
            fn new(state: agent_core::AdmittedMediaSnapshot, room: &Arc<RoomRef>) -> Self {
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
            #[cfg(target_arch = "wasm32")]
            room: Arc<RoomRef>,
            #[cfg(target_arch = "wasm32")]
            admission: agent_core::AdmittedMedia,
        }

        #[uniffi::export]
        impl $track {
            pub fn same_admission(&self, other: Arc<Self>) -> bool {
                #[cfg(target_arch = "wasm32")]
                {
                    Arc::ptr_eq(&self.room.agent, &other.room.agent)
                        && self.admission == other.admission
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let _ = other;
                    false
                }
            }

            pub async fn playback_output(&self) -> Result<Arc<$output>, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    let observation = self
                        .room
                        .agent
                        .runtime
                        .observe_admitted_media(self.admission.clone())
                        .await?;
                    Ok(Arc::new($output {
                        output: playback::Output::new(observation)?,
                    }))
                }
                #[cfg(not(target_arch = "wasm32"))]
                Err(crate::ffi::browser_required())
            }
        }

        #[derive(uniffi::Object)]
        pub struct $updates {
            #[cfg(target_arch = "wasm32")]
            room: Arc<RoomRef>,
            #[cfg(target_arch = "wasm32")]
            cursor: agent_core::observer::MediaCollectionUpdates,
        }

        #[uniffi::export]
        impl $updates {
            pub async fn next(&self) -> Result<Option<$state>, WebAgentError> {
                #[cfg(target_arch = "wasm32")]
                {
                    let next = self.cursor.next().map_err(|_| {
                        ErrorInfo::new(
                            ErrorCode::Conflict,
                            "Collection updates already have a pending read",
                        )
                    })?;
                    Ok(next.await.map(|state| $state::new(state, &self.room)))
                }
                #[cfg(not(target_arch = "wasm32"))]
                Err(crate::ffi::browser_required())
            }

            pub fn close(&self) {
                #[cfg(target_arch = "wasm32")]
                self.cursor.close();
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
    WebVideoOutput
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
    WebAudioOutput
);
