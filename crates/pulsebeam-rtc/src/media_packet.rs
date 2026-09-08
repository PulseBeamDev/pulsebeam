#![allow(
    clippy::disallowed_types,
    reason = "the v3 media-value contract requires immutable shared packet bytes and metadata"
)]

use std::{cell::Cell, marker::PhantomData, ops::Range, sync::Arc};

use bytes::Bytes;

use crate::{FrameId, GlobalMediaTime};

#[derive(Clone, Debug)]
pub struct MediaPacket {
    bytes: Bytes,
    global_media_at: GlobalMediaTime,
    extensions: Arc<[(Arc<str>, Range<usize>)]>,
    _not_sync: PhantomData<Cell<()>>,
}

impl MediaPacket {
    #[allow(dead_code, reason = "packets are constructed by ingress in Plan 03")]
    pub(crate) fn new(
        bytes: Bytes,
        global_media_at: GlobalMediaTime,
        extensions: Arc<[(Arc<str>, Range<usize>)]>,
    ) -> Self {
        Self {
            bytes,
            global_media_at,
            extensions,
            _not_sync: PhantomData,
        }
    }

    pub const fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub const fn global_media_at(&self) -> GlobalMediaTime {
        self.global_media_at
    }

    pub fn extension(&self, uri: &str) -> Option<&[u8]> {
        let (_, range) = self
            .extensions
            .iter()
            .find(|(candidate, _)| &**candidate == uri)?;
        self.bytes.get(range.clone())
    }

    pub fn to_transit(&self) -> Self {
        Self {
            bytes: Bytes::copy_from_slice(&self.bytes),
            global_media_at: self.global_media_at,
            extensions: Arc::clone(&self.extensions),
            _not_sync: PhantomData,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ForwardedMedia {
    pub packet: MediaPacket,
    pub frame: FrameMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameMetadata {
    pub id: FrameId,
    pub boundary: FrameBoundary,
    pub random_access: bool,
    pub discardable: bool,
    pub dependencies: FrameDependencies,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameBoundary {
    Complete,
    Start,
    Middle,
    End,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameDependencies {
    Unknown,
    Known(Arc<[FrameId]>),
}

impl FrameDependencies {
    pub const MAX_DIRECT_DEPENDENCIES: usize = 8;

    pub fn known(dependencies: impl Into<Arc<[FrameId]>>) -> Option<Self> {
        let dependencies = dependencies.into();
        (dependencies.len() <= Self::MAX_DIRECT_DEPENDENCIES).then_some(Self::Known(dependencies))
    }
}
