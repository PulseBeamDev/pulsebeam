use super::*;
use pulsebeam_proto::catalog::{MAX_CATALOG_PAGE_BYTES, MAX_CATALOG_PAGE_RECORDS};

pub(crate) const MAX_WATCHES: usize = 8;
const MAX_ERRORS: usize = codec::MAX_RECORDS;
const SESSION_BYTES: usize = 16 * 1024 * 1024;

#[cfg(test)]
mod tests;

struct Watch {
    request: wire::CatalogPageRequest,
    revision: u64,
    output: VecDeque<wire::CatalogServerRecord>,
    bytes: usize,
}

#[derive(Default)]
pub(crate) struct CatalogSession {
    watches: BTreeMap<u64, Watch>,
    errors: VecDeque<wire::CatalogServerRecord>,
    last_id: u64,
    output_after: u64,
}

impl CatalogSession {
    pub(crate) fn input(&mut self, record: wire::CatalogClientRecord) -> Result<(), OutputError> {
        match record.command.ok_or(OutputError::InvalidObservation)? {
            wire::catalog_client_record::Command::Open(mut request) => {
                request.participant_limit = pulsebeam_proto::catalog::validate_request(
                    request.watch_id,
                    &request.room,
                    request.participant_limit,
                    &request.cursor,
                )
                .map_err(|_| OutputError::InvalidObservation)?;
                let id = request.watch_id;
                if let Some(watch) = self.watches.get(&id) {
                    if watch.request == request {
                        return Ok(());
                    }
                    return self.fail(id, wire::FailureCode::InvalidConfiguration);
                }
                if id <= self.last_id {
                    return self.fail(id, wire::FailureCode::ResourceGone);
                }
                self.last_id = id;
                if self.watches.len() == MAX_WATCHES {
                    return self.fail(id, wire::FailureCode::CapacityExceeded);
                }
                self.watches.insert(
                    id,
                    Watch {
                        request,
                        revision: 0,
                        output: VecDeque::new(),
                        bytes: 0,
                    },
                );
            }
            wire::catalog_client_record::Command::Close(close) => {
                self.last_id = self.last_id.max(close.watch_id);
                self.watches.remove(&close.watch_id);
            }
        }
        Ok(())
    }

    pub(crate) fn requests(&self) -> impl Iterator<Item = &wire::CatalogPageRequest> {
        self.watches.values().map(|watch| &watch.request)
    }

    pub(crate) fn contains(&self, id: u64) -> bool {
        self.watches.contains_key(&id)
    }

    pub(crate) fn fail(&mut self, id: u64, code: wire::FailureCode) -> Result<(), OutputError> {
        self.watches.remove(&id);
        if self.errors.len() == MAX_ERRORS {
            return Err(OutputError::Capacity);
        }
        self.errors.push_back(wire::CatalogServerRecord {
            response: Some(wire::catalog_server_record::Response::Error(
                wire::CatalogWatchError {
                    watch_id: id,
                    failure: Some(wire::Failure {
                        code: code as i32,
                        diagnostic: String::new(),
                    }),
                },
            )),
        });
        Ok(())
    }

    pub(crate) fn replace(
        &mut self,
        id: u64,
        revision: u64,
        records: Vec<wire::CatalogPageRecord>,
    ) -> Result<(), OutputError> {
        let Some(watch) = self.watches.get(&id) else {
            return Ok(());
        };
        if revision <= watch.revision {
            return Ok(());
        }
        let bytes = records.iter().try_fold(0usize, |total, record| {
            if record.watch_id != id
                || record.revision != revision
                || record.room != watch.request.room
            {
                return Err(OutputError::InvalidObservation);
            }
            pulsebeam_proto::catalog::encode_page_record(record)
                .map_err(|_| OutputError::InvalidObservation)?;
            total
                .checked_add(record.encoded_len())
                .ok_or(OutputError::Capacity)
        })?;
        let retained = self
            .watches
            .values()
            .fold(0usize, |total, watch| total.saturating_add(watch.bytes));
        if records.len() > MAX_CATALOG_PAGE_RECORDS
            || bytes > MAX_CATALOG_PAGE_BYTES
            || retained.saturating_sub(watch.bytes).saturating_add(bytes) > SESSION_BYTES / 2
        {
            return self.fail(id, wire::FailureCode::CapacityExceeded);
        }
        let watch = self
            .watches
            .get_mut(&id)
            .ok_or(OutputError::InvalidObservation)?;
        watch.revision = revision;
        watch.bytes = bytes;
        watch.output = records
            .into_iter()
            .map(|page| wire::CatalogServerRecord {
                response: Some(wire::catalog_server_record::Response::Page(page)),
            })
            .collect();
        Ok(())
    }

    pub(crate) fn append(&mut self, batch: &mut wire::ServerStateBatch) {
        for _ in 0..codec::MAX_RECORDS / 2 {
            if batch
                .records
                .len()
                .saturating_add(batch.catalog_records.len())
                == codec::MAX_RECORDS
            {
                break;
            }
            let next = if let Some(error) = self.errors.front() {
                Some((None, error))
            } else {
                self.watches
                    .range((Excluded(self.output_after), Unbounded))
                    .chain(self.watches.range(..=self.output_after))
                    .find_map(|(id, watch)| watch.output.front().map(|record| (Some(*id), record)))
            };
            let Some((id, record)) = next else { break };
            batch.catalog_records.push(record.clone());
            if batch.encoded_len() > codec::MAX_FRAME_SIZE {
                batch.catalog_records.pop();
                break;
            }
            if let Some(id) = id {
                if let Some(watch) = self.watches.get_mut(&id)
                    && let Some(record) = watch.output.pop_front()
                    && let Some(wire::catalog_server_record::Response::Page(page)) = record.response
                {
                    watch.bytes = watch.bytes.saturating_sub(page.encoded_len());
                }
                self.output_after = id;
            } else {
                self.errors.pop_front();
            }
        }
    }

    pub(crate) fn restore_unsent(
        &mut self,
        records: Vec<wire::CatalogServerRecord>,
    ) -> Result<(), OutputError> {
        for record in records.into_iter().rev() {
            match record.response.as_ref() {
                Some(wire::catalog_server_record::Response::Page(page)) => {
                    let Some(watch) = self.watches.get_mut(&page.watch_id) else {
                        continue;
                    };
                    if page.revision != watch.revision || page.room != watch.request.room {
                        continue;
                    }
                    watch.bytes = watch
                        .bytes
                        .checked_add(page.encoded_len())
                        .ok_or(OutputError::Capacity)?;
                    if watch.bytes > MAX_CATALOG_PAGE_BYTES
                        || watch.output.len() >= MAX_CATALOG_PAGE_RECORDS
                    {
                        return Err(OutputError::Capacity);
                    }
                    watch.output.push_front(record);
                }
                Some(wire::catalog_server_record::Response::Error(_)) => {
                    if self.errors.len() == MAX_ERRORS {
                        return Err(OutputError::Capacity);
                    }
                    self.errors.push_front(record);
                }
                None => return Err(OutputError::InvalidObservation),
            }
        }
        Ok(())
    }

    pub(crate) fn pending(&self) -> bool {
        !self.errors.is_empty() || self.watches.values().any(|watch| !watch.output.is_empty())
    }

    pub(crate) fn invalidate(&mut self) {
        for watch in self.watches.values_mut() {
            watch.output.clear();
            watch.bytes = 0;
        }
    }

    pub(crate) fn close(&mut self) {
        self.watches.clear();
        self.errors.clear();
    }
}
