// This isolated FFI boundary shares immutable, complete pages across at most 32 watch
// cursors. It has no packet callers or mutable shared state. UniFFI emits Arc sibling
// implementations, so this exception must include the generated module items.
#![allow(clippy::disallowed_types)]

use alloc::{string::String, sync::Arc, vec::Vec};

use super::{EffectiveMediaFormat, ErrorInfo, TopicMode};

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct WatchConfig {
    pub limit: u32,
    pub cursor: Vec<u8>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            limit: 100,
            cursor: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum WatchStatus {
    Loading,
    Live,
    Failed,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum CatalogPublicationKind {
    Audio,
    Video,
    Data { delivery: TopicMode },
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct CatalogPublicationInfo {
    pub id: String,
    pub track_id: String,
    pub kind: CatalogPublicationKind,
    pub effective_format: Option<EffectiveMediaFormat>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct CatalogParticipantInfo {
    pub id: String,
    pub publications: Vec<CatalogPublicationInfo>,
}

#[derive(Debug, PartialEq, Eq, uniffi::Object)]
pub struct CatalogPage {
    pub(crate) revision: u64,
    pub(crate) participants: Vec<CatalogParticipantInfo>,
    pub(crate) has_more: bool,
    pub(crate) next_cursor: Vec<u8>,
    pub(crate) cursor_expires_at_unix_seconds: i64,
}

impl CatalogPage {
    pub(crate) fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

#[uniffi::export]
impl CatalogPage {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn participants(&self) -> Vec<CatalogParticipantInfo> {
        self.participants.clone()
    }
    pub fn has_more(&self) -> bool {
        self.has_more
    }
    pub fn next_cursor(&self) -> Vec<u8> {
        self.next_cursor.clone()
    }
    pub fn cursor_expires_at_unix_seconds(&self) -> i64 {
        self.cursor_expires_at_unix_seconds
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct WatchState {
    pub observation_revision: u64,
    pub room: String,
    pub limit: u32,
    pub cursor: Vec<u8>,
    pub status: WatchStatus,
    pub page: Option<Arc<CatalogPage>>,
    pub error: Option<ErrorInfo>,
}

impl WatchState {
    pub(crate) fn close(&mut self) {
        if self.status != WatchStatus::Failed {
            self.status = WatchStatus::Closed;
        }
        self.page = None;
    }
}
