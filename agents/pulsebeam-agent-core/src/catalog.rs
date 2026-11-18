use crate::{
    Generation, TrackOwner,
    ffi::{
        CatalogPage, CatalogParticipantInfo, CatalogPublicationInfo, CatalogPublicationKind,
        ErrorCode, ErrorInfo, WatchConfig, WatchState, WatchStatus,
    },
};
use alloc::{collections::BTreeMap, string::String, vec::Vec};
use pulsebeam_proto::{
    catalog::{self, CatalogAssembler},
    state as wire,
};

const MAX_WATCHES: usize = 8;

struct Watch {
    state: WatchState,
    wire: Option<u64>,
    assembler: Option<CatalogAssembler>,
    assembling_revision: u64,
    committed_revision: u64,
}

#[derive(Default)]
pub(crate) struct Catalogs {
    watches: BTreeMap<TrackOwner, Watch>,
    generation: Option<Generation>,
    next_id: u64,
    pending: BTreeMap<u64, wire::CatalogClientRecord>,
}

impl Catalogs {
    pub(crate) fn open(
        &mut self,
        owner: TrackOwner,
        room: String,
        config: WatchConfig,
    ) -> Result<WatchState, ErrorInfo> {
        let limit = catalog::validate_request(1, &room, config.limit, &config.cursor)
            .map_err(|_| ErrorInfo::new(ErrorCode::InvalidArgument, "invalid catalog scope"))?;
        if room.contains('*') {
            return Err(ErrorInfo::new(
                ErrorCode::InvalidArgument,
                "catalog room must be literal",
            ));
        }
        if self.watches.contains_key(&owner) {
            return Err(ErrorInfo::new(
                ErrorCode::Conflict,
                "watch owner already in use",
            ));
        }
        if self.watches.len() >= MAX_WATCHES {
            return Err(ErrorInfo::new(
                ErrorCode::CapacityExceeded,
                "catalog watch capacity exceeded",
            ));
        }
        let state = WatchState {
            observation_revision: 1,
            room,
            limit,
            cursor: config.cursor,
            status: WatchStatus::Loading,
            page: None,
            error: None,
        };
        self.watches.insert(
            owner,
            Watch {
                state: state.clone(),
                wire: None,
                assembler: None,
                assembling_revision: 0,
                committed_revision: 0,
            },
        );
        Ok(state)
    }

    pub(crate) fn state(&self, owner: TrackOwner) -> Option<WatchState> {
        self.watches.get(&owner).map(|watch| watch.state.clone())
    }

    pub(crate) fn close(&mut self, owner: TrackOwner) -> Option<WatchState> {
        let mut watch = self.watches.remove(&owner)?;
        if let Some(id) = watch.wire {
            let unsent = self.pending.remove(&id).is_some_and(|record| {
                matches!(
                    record.command,
                    Some(wire::catalog_client_record::Command::Open(_))
                )
            });
            if !unsent {
                self.pending.insert(
                    id,
                    wire::CatalogClientRecord {
                        command: Some(wire::catalog_client_record::Command::Close(
                            wire::CatalogWatchClose { watch_id: id },
                        )),
                    },
                );
            }
        }
        watch.state.close();
        watch.state.observation_revision = watch.state.observation_revision.saturating_add(1);
        Some(watch.state)
    }

    pub(crate) fn disconnect(&mut self) {
        self.generation = None;
        self.pending.clear();
        for watch in self.watches.values_mut() {
            if watch.state.status != WatchStatus::Loading || watch.state.page.is_some() {
                watch.state.observation_revision =
                    watch.state.observation_revision.saturating_add(1);
            }
            watch.state.status = WatchStatus::Loading;
            watch.state.page = None;
            watch.wire = None;
            watch.assembler = None;
            watch.assembling_revision = 0;
            watch.committed_revision = 0;
        }
    }

    pub(crate) fn reconcile(
        &mut self,
        generation: Generation,
        state: &mut crate::native_state::NativeState,
    ) -> Result<(), crate::SignalingError> {
        if self.generation != Some(generation) {
            self.disconnect();
            self.generation = Some(generation);
            self.next_id = 0;
        }
        for watch in self
            .watches
            .values_mut()
            .filter(|watch| watch.wire.is_none())
        {
            self.next_id = self
                .next_id
                .checked_add(1)
                .ok_or(crate::SignalingError::Encode)?;
            let id = self.next_id;
            watch.wire = Some(id);
            self.pending.insert(
                id,
                wire::CatalogClientRecord {
                    command: Some(wire::catalog_client_record::Command::Open(
                        wire::CatalogPageRequest {
                            watch_id: id,
                            room: watch.state.room.clone(),
                            participant_limit: watch.state.limit,
                            cursor: watch.state.cursor.clone(),
                        },
                    )),
                },
            );
        }
        while let Some((_, record)) = self.pending.pop_first() {
            state.queue_catalog(record)?;
        }
        Ok(())
    }

    pub(crate) fn apply(
        &mut self,
        record: wire::CatalogServerRecord,
    ) -> Option<(TrackOwner, WatchState)> {
        let id = match record.response.as_ref()? {
            wire::catalog_server_record::Response::Page(page) => page.watch_id,
            wire::catalog_server_record::Response::Error(error) => error.watch_id,
        };
        let owner = *self
            .watches
            .iter()
            .find(|(_, watch)| watch.wire == Some(id))?
            .0;
        let watch = self.watches.get_mut(&owner)?;
        let failure = match record.response? {
            wire::catalog_server_record::Response::Error(error) => error
                .failure
                .as_ref()
                .map(|failure| {
                    let code = match failure.code() {
                        wire::FailureCode::InvalidConfiguration => ErrorCode::InvalidArgument,
                        wire::FailureCode::PermissionDenied => ErrorCode::PermissionDenied,
                        wire::FailureCode::Incompatible | wire::FailureCode::Unsupported => {
                            ErrorCode::Unsupported
                        }
                        wire::FailureCode::CapacityExceeded => ErrorCode::CapacityExceeded,
                        wire::FailureCode::ResourceGone => ErrorCode::Closed,
                        _ => ErrorCode::Internal,
                    };
                    ErrorInfo::new(code, "Catalog watch failed")
                })
                .unwrap_or_else(|| ErrorInfo::new(ErrorCode::Internal, "invalid catalog failure")),
            wire::catalog_server_record::Response::Page(page) => {
                if page.revision <= watch.committed_revision
                    || page.revision < watch.assembling_revision
                {
                    return None;
                }
                let before = watch.state.observation_revision;
                match Self::assemble(watch, page) {
                    Ok(()) => {
                        return (watch.state.observation_revision != before)
                            .then(|| (owner, watch.state.clone()));
                    }
                    Err(error) => error,
                }
            }
        };
        let mut watch = self.watches.remove(&owner)?;
        self.pending.insert(
            id,
            wire::CatalogClientRecord {
                command: Some(wire::catalog_client_record::Command::Close(
                    wire::CatalogWatchClose { watch_id: id },
                )),
            },
        );
        watch.state.page = None;
        watch.state.status = WatchStatus::Failed;
        watch.state.error = Some(failure);
        watch.state.observation_revision = watch.state.observation_revision.saturating_add(1);
        Some((owner, watch.state))
    }

    fn assemble(watch: &mut Watch, page: wire::CatalogPageRecord) -> Result<(), ErrorInfo> {
        let invalid = || ErrorInfo::new(ErrorCode::Internal, "invalid catalog page");
        if page.revision > watch.assembling_revision {
            watch.assembler = Some(
                CatalogAssembler::new(page.watch_id, watch.state.room.clone(), page.revision)
                    .map_err(|_| invalid())?,
            );
            watch.assembling_revision = page.revision;
        }
        let assembler = watch.assembler.as_mut().ok_or_else(invalid)?;
        assembler.push_record(page).map_err(|_| invalid())?;
        let Some(page) = assembler.finish_page().map_err(|_| invalid())? else {
            return Ok(());
        };
        if page.entries.len() > usize::try_from(watch.state.limit).map_err(|_| invalid())? {
            return Err(invalid());
        }
        let participants = page
            .entries
            .into_iter()
            .map(|entry| {
                let participant = entry.participant.ok_or_else(invalid)?;
                let publications = entry
                    .tracks
                    .into_iter()
                    .map(|track| {
                        let kind = match track.kind.ok_or_else(invalid)? {
                            wire::track::Kind::Audio(_) => CatalogPublicationKind::Audio,
                            wire::track::Kind::Video(_) => CatalogPublicationKind::Video,
                            wire::track::Kind::Data(data) => CatalogPublicationKind::Data {
                                delivery: match wire::DataDelivery::try_from(data.delivery)
                                    .map_err(|_| invalid())?
                                {
                                    wire::DataDelivery::Latest => crate::ffi::TopicMode::Latest,
                                    wire::DataDelivery::Lossy => crate::ffi::TopicMode::Lossy,
                                    wire::DataDelivery::Ordered => crate::ffi::TopicMode::Ordered,
                                    _ => return Err(invalid()),
                                },
                            },
                        };
                        Ok(CatalogPublicationInfo {
                            id: track.track_id,
                            label: track.label,
                            kind,
                        })
                    })
                    .collect::<Result<Vec<_>, ErrorInfo>>()?;
                Ok(CatalogParticipantInfo {
                    id: participant.participant_id,
                    external_id: participant.participant_external_id,
                    publications,
                })
            })
            .collect::<Result<Vec<_>, ErrorInfo>>()?;
        watch.committed_revision = page.revision;
        watch.assembler = None;
        watch.state.status = WatchStatus::Live;
        watch.state.page = Some(
            CatalogPage {
                revision: page.revision,
                participants,
                has_more: page.has_more,
                next_cursor: page.next_cursor,
                cursor_expires_at_unix_seconds: page.cursor_expires_at_unix_seconds,
            }
            .shared(),
        );
        watch.state.observation_revision = watch
            .state
            .observation_revision
            .checked_add(1)
            .ok_or_else(|| {
                ErrorInfo::new(ErrorCode::CapacityExceeded, "catalog revision exhausted")
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
