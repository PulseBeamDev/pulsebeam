use super::{
    Arc, ErrorCode, ErrorInfo, WebAgentError,
    video::{Participant, RoomRef},
};
use agent_core::ffi;

#[uniffi::export]
impl RoomRef {
    #[uniffi::method(default(options = None))]
    pub fn watch(
        self: Arc<Self>,
        options: Option<ffi::WatchConfig>,
    ) -> Result<Arc<Catalog>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let runtime = self
                .agent
                .runtime
                .open_watch(self.room.clone(), options.unwrap_or_default())?;
            Ok(Arc::new(Catalog {
                runtime,
                room: self,
            }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = options;
            Err(super::browser_required())
        }
    }
}

#[derive(uniffi::Object)]
pub struct Catalog {
    #[cfg(target_arch = "wasm32")]
    runtime: crate::browser::catalog::BrowserCatalogWatch,
    #[cfg(target_arch = "wasm32")]
    room: Arc<RoomRef>,
}

#[uniffi::export]
impl Catalog {
    pub fn state(&self) -> Result<CatalogWatchState, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            Ok(CatalogWatchState::new(
                self.runtime.current_state(),
                &self.room,
            ))
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(super::browser_required())
    }

    pub fn updates(&self) -> Result<Arc<CatalogUpdates>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let cursor = self.runtime.open_updates()?;
            Ok(Arc::new(CatalogUpdates {
                cursor,
                room: self.room.clone(),
            }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(super::browser_required())
    }

    pub async fn close(&self) {
        #[cfg(target_arch = "wasm32")]
        self.runtime.close();
    }
}

#[derive(uniffi::Object)]
pub struct CatalogUpdates {
    cursor: agent_core::observer::WatchUpdates,
    room: Arc<RoomRef>,
}

#[uniffi::export]
impl CatalogUpdates {
    pub async fn next(&self) -> Result<Option<CatalogWatchState>, WebAgentError> {
        let next = self.cursor.next().map_err(|_| {
            ErrorInfo::new(
                ErrorCode::Conflict,
                "An observation read is already pending",
            )
        })?;
        Ok(next
            .await
            .map(|state| CatalogWatchState::new(state, &self.room)))
    }

    pub fn close(&self) {
        self.cursor.close();
    }
}

#[derive(uniffi::Record)]
pub struct CatalogWatchState {
    pub observation_revision: u64,
    pub room: String,
    pub limit: u32,
    pub cursor: Vec<u8>,
    pub status: ffi::WatchStatus,
    pub page: Option<Arc<WebCatalogPage>>,
    pub error: Option<ffi::ErrorInfo>,
}

impl CatalogWatchState {
    fn new(state: ffi::WatchState, room: &Arc<RoomRef>) -> Self {
        Self {
            observation_revision: state.observation_revision,
            room: state.room,
            limit: state.limit,
            cursor: state.cursor,
            status: state.status,
            page: state.page.map(|page| {
                Arc::new(WebCatalogPage {
                    page,
                    room: room.clone(),
                })
            }),
            error: state.error,
        }
    }
}

#[derive(uniffi::Object)]
pub struct WebCatalogPage {
    page: Arc<ffi::CatalogPage>,
    room: Arc<RoomRef>,
}

#[derive(uniffi::Object)]
pub struct CatalogParticipant {
    data_recipient_id: Option<String>,
    tracks: Vec<ffi::CatalogPublicationInfo>,
    reference: Arc<Participant>,
}

#[uniffi::export]
impl CatalogParticipant {
    pub fn id(&self) -> String {
        self.reference.id().into()
    }

    pub fn data_recipient_id(&self) -> Option<String> {
        self.data_recipient_id.clone()
    }

    pub fn tracks(&self) -> Vec<ffi::CatalogPublicationInfo> {
        self.tracks.clone()
    }

    pub fn video(
        &self,
        label: String,
    ) -> Result<Arc<super::video::RemoteVideoTrack>, WebAgentError> {
        self.reference.video(label)
    }

    pub fn audio(
        &self,
        label: String,
    ) -> Result<Arc<super::video::RemoteAudioTrack>, WebAgentError> {
        self.reference.audio(label)
    }

    pub fn data(&self, label: String) -> Result<Arc<super::data::RemoteDataTrack>, WebAgentError> {
        self.reference.data(label)
    }
}

#[uniffi::export]
impl WebCatalogPage {
    pub fn revision(&self) -> u64 {
        self.page.revision()
    }

    pub fn participants(&self) -> Result<Vec<Arc<CatalogParticipant>>, WebAgentError> {
        self.page
            .participants()
            .into_iter()
            .map(|entry| {
                let reference = self.room.participant(entry.id.clone())?;
                Ok(Arc::new(CatalogParticipant {
                    data_recipient_id: entry.data_recipient_id,
                    tracks: entry.publications,
                    reference,
                }))
            })
            .collect()
    }

    pub fn has_more(&self) -> bool {
        self.page.has_more()
    }

    pub fn next_cursor(&self) -> Vec<u8> {
        self.page.next_cursor()
    }

    pub fn cursor_expires_at_unix_seconds(&self) -> i64 {
        self.page.cursor_expires_at_unix_seconds()
    }
}
