//! Bounded, standalone RFC288 catalog records and atomic participant assembly.
use crate::state::validation::is_application_name;
use alloc::{string::String, vec, vec::Vec};
use prost::Message;

use crate::state::{
    CatalogPageRecord, CatalogParticipantEntry, CatalogParticipantFragment, LogicalSourceAddress,
};

pub const DEFAULT_CATALOG_PAGE_SIZE: u32 = 100;
pub const MAX_CATALOG_PAGE_SIZE: u32 = 500;
pub const MAX_TRACKS_PER_PARTICIPANT: usize = 64;
pub const MAX_CATALOG_RECORD_BYTES: usize = 4096;
pub const MAX_CATALOG_RECORDS: usize = 16;
const MAX_CATALOG_FRAME_BYTES: usize = crate::state::codec::MAX_FRAME_SIZE;
pub const MAX_CATALOG_ENTRY_BYTES: usize = 64 * 1024;
pub const MAX_CATALOG_ENTRY_FRAGMENTS: usize = 16;
pub const MAX_CATALOG_CURSOR_BYTES: usize = 256;
pub const MAX_CATALOG_PAGE_BYTES: usize = 1024 * 1024;
pub const MAX_CATALOG_PAGE_RECORDS: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogError {
    InvalidAddress,
    InvalidRequest,
    InvalidRecord,
    RecordTooLarge,
    TooManyRecords,
    EntryTooLarge,
    TooManyFragments,
    TooManyTracks,
    PageTooLarge,
    InvalidEntry,
    Inconsistent,
    Duplicate,
    StaleRevision,
    OutOfOrder,
}

/// Validate the exact logical selector without normalizing or changing identity.
pub fn validate_address(address: &LogicalSourceAddress) -> Result<(), CatalogError> {
    if !is_application_name(&address.room)
        || !is_application_name(&address.participant_id)
        || !is_application_name(&address.track_id)
        || !matches!(address.kind, 1..=3)
    {
        return Err(CatalogError::InvalidAddress);
    }
    Ok(())
}

/// Validate the scoped first-page/continuation request. Cursor contents are
/// opaque, bounded bytes; the server binds them to room, ordering and expiry.
pub fn validate_request(
    watch_id: u64,
    room: &str,
    participant_limit: u32,
    cursor: &[u8],
) -> Result<u32, CatalogError> {
    if watch_id == 0
        || !is_application_name(room)
        || (participant_limit != 0 && !(1..=MAX_CATALOG_PAGE_SIZE).contains(&participant_limit))
        || cursor.len() > MAX_CATALOG_CURSOR_BYTES
    {
        return Err(CatalogError::InvalidRequest);
    }
    Ok(if participant_limit == 0 {
        DEFAULT_CATALOG_PAGE_SIZE
    } else {
        participant_limit
    })
}

/// A standalone page record must fit its complete protobuf representation,
/// including all repeated-fragment tags and lengths. It has at most 16 records.
pub fn encode_page_record(record: &CatalogPageRecord) -> Result<Vec<u8>, CatalogError> {
    validate_page_record(record)?;
    let size = record.encoded_len();
    if size > MAX_CATALOG_RECORD_BYTES {
        return Err(CatalogError::RecordTooLarge);
    }
    Ok(record.encode_to_vec())
}

pub fn decode_page_record(bytes: &[u8]) -> Result<CatalogPageRecord, CatalogError> {
    if bytes.is_empty() || bytes.len() > MAX_CATALOG_RECORD_BYTES {
        return Err(CatalogError::RecordTooLarge);
    }
    let record = CatalogPageRecord::decode(bytes).map_err(|_| CatalogError::InvalidRecord)?;
    validate_page_record(&record)?;
    Ok(record)
}

pub(crate) fn validate_page_record(record: &CatalogPageRecord) -> Result<(), CatalogError> {
    if record.watch_id == 0
        || record.revision == 0
        || !is_application_name(&record.room)
        || record.fragments.len() > MAX_CATALOG_RECORDS
        || record.next_cursor.len() > MAX_CATALOG_CURSOR_BYTES
        || record.has_more != !record.next_cursor.is_empty()
        || record.has_more && record.cursor_expires_at_unix_seconds <= 0
        || !record.has_more && record.cursor_expires_at_unix_seconds != 0
        || record.complete && record.participant_index_out_of_range()
    {
        return Err(CatalogError::InvalidRecord);
    }
    for fragment in &record.fragments {
        validate_fragment(fragment)?;
    }
    if record.encoded_len() > MAX_CATALOG_RECORD_BYTES {
        return Err(CatalogError::RecordTooLarge);
    }
    Ok(())
}

impl CatalogPageRecord {
    fn participant_index_out_of_range(&self) -> bool {
        self.fragments
            .iter()
            .any(|fragment| fragment.participant_index >= MAX_CATALOG_PAGE_SIZE)
    }
}

fn validate_fragment(fragment: &CatalogParticipantFragment) -> Result<(), CatalogError> {
    if fragment.watch_id == 0
        || fragment.revision == 0
        || !is_application_name(&fragment.participant_id)
        || fragment.participant_index >= MAX_CATALOG_PAGE_SIZE
        || fragment.fragment_count == 0
        || fragment.fragment_count as usize > MAX_CATALOG_ENTRY_FRAGMENTS
        || fragment.fragment_index >= fragment.fragment_count
    {
        return Err(CatalogError::InvalidRecord);
    }
    // Include the field-8 tag and length prefix used inside CatalogPageRecord.
    let nested_size = 1_usize
        .checked_add(varint_len(fragment.encoded_len()))
        .and_then(|size| size.checked_add(fragment.encoded_len()))
        .unwrap_or(usize::MAX);
    if nested_size > MAX_CATALOG_RECORD_BYTES {
        return Err(CatalogError::RecordTooLarge);
    }
    Ok(())
}

// Account for CatalogPageRecord -> CatalogServerRecord(page) ->
// ServerStateBatch.catalog_records, plus initial_complete=true.
fn catalog_page_frame_len(page_len: usize, initial_complete: bool) -> Option<usize> {
    let server_record_len = 1usize
        .checked_add(varint_len(page_len))?
        .checked_add(page_len)?;
    let batch_record_len = 1usize
        .checked_add(varint_len(server_record_len))?
        .checked_add(server_record_len)?;
    batch_record_len.checked_add(if initial_complete { 2 } else { 0 })
}

fn varint_len(mut value: usize) -> usize {
    let mut bytes: usize = 1;
    while value >= 128 {
        value >>= 7;
        bytes = bytes.saturating_add(1);
    }
    bytes
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogPageMetadata {
    pub watch_id: u64,
    pub room: String,
    pub revision: u64,
    pub complete: bool,
    pub has_more: bool,
    pub next_cursor: Vec<u8>,
    pub cursor_expires_at_unix_seconds: i64,
}

/// Encode one whole entry into at most 16 individually bounded page records.
/// This accounts for nested protobuf framing, not only the chunk payload.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "binary-search offsets are bounded by the checked 64 KiB entry and 4 KiB record ceilings"
)]
pub fn fragment_entry(
    metadata: &CatalogPageMetadata,
    participant_index: u32,
    entry: &CatalogParticipantEntry,
) -> Result<Vec<CatalogPageRecord>, CatalogError> {
    if entry.tracks.len() > MAX_TRACKS_PER_PARTICIPANT {
        return Err(CatalogError::TooManyTracks);
    }
    if metadata.watch_id == 0
        || metadata.revision == 0
        || !is_application_name(&metadata.room)
        || metadata.next_cursor.len() > MAX_CATALOG_CURSOR_BYTES
        || metadata.has_more != !metadata.next_cursor.is_empty()
        || metadata.has_more && metadata.cursor_expires_at_unix_seconds <= 0
        || !metadata.has_more && metadata.cursor_expires_at_unix_seconds != 0
        || participant_index >= MAX_CATALOG_PAGE_SIZE
    {
        return Err(CatalogError::InvalidRecord);
    }
    let participant = entry
        .participant
        .as_ref()
        .ok_or(CatalogError::InvalidEntry)?;
    if participant.room_id != metadata.room
        || crate::state::validation::validate_participant(participant).is_err()
        || entry.tracks.iter().any(|track| {
            track.participant_id != participant.participant_id
                || track.room_id != participant.room_id
                || crate::state::validation::validate_track(track).is_err()
        })
    {
        return Err(CatalogError::InvalidEntry);
    }
    if entry.encoded_len() > MAX_CATALOG_ENTRY_BYTES {
        return Err(CatalogError::EntryTooLarge);
    }
    let entry_bytes = entry.encode_to_vec();
    let participant_id = participant.participant_id.clone();
    let mut records = Vec::new();
    let mut offset = 0;
    while offset < entry_bytes.len() {
        if records.len() == MAX_CATALOG_ENTRY_FRAGMENTS {
            return Err(CatalogError::TooManyFragments);
        }
        let index = u32::try_from(records.len()).map_err(|_| CatalogError::TooManyFragments)?;
        let mut low = 1;
        let mut high = (entry_bytes.len() - offset).min(MAX_CATALOG_RECORD_BYTES);
        let mut accepted = None;
        while low <= high {
            let size = low + (high - low) / 2;
            let fragment = CatalogParticipantFragment {
                watch_id: metadata.watch_id,
                revision: metadata.revision,
                participant_index,
                fragment_index: index,
                fragment_count: 1,
                participant_id: participant_id.clone(),
                entry_chunk: entry_bytes
                    .get(offset..offset + size)
                    .ok_or(CatalogError::InvalidEntry)?
                    .to_vec(),
            };
            let record = CatalogPageRecord {
                watch_id: metadata.watch_id,
                room: metadata.room.clone(),
                revision: metadata.revision,
                complete: metadata.complete && offset + size == entry_bytes.len(),
                has_more: metadata.has_more,
                next_cursor: metadata.next_cursor.clone(),
                cursor_expires_at_unix_seconds: metadata.cursor_expires_at_unix_seconds,
                fragments: vec![fragment],
            };
            if record.encoded_len() <= MAX_CATALOG_RECORD_BYTES
                && catalog_page_frame_len(record.encoded_len(), true)
                    .is_some_and(|len| len <= MAX_CATALOG_FRAME_BYTES)
            {
                accepted = Some((size, record));
                low = size + 1;
            } else {
                high = size - 1;
            }
        }
        let (size, record) = accepted.ok_or(CatalogError::RecordTooLarge)?;
        records.push(record);
        offset += size;
    }
    let count = u32::try_from(records.len()).map_err(|_| CatalogError::TooManyFragments)?;
    for (index, record) in records.iter_mut().enumerate() {
        let fragment = record
            .fragments
            .first_mut()
            .ok_or(CatalogError::InvalidRecord)?;
        fragment.fragment_index =
            u32::try_from(index).map_err(|_| CatalogError::TooManyFragments)?;
        fragment.fragment_count = count;
        if record.encoded_len() > MAX_CATALOG_RECORD_BYTES
            || catalog_page_frame_len(record.encoded_len(), true)
                .is_none_or(|len| len > MAX_CATALOG_FRAME_BYTES)
        {
            return Err(CatalogError::RecordTooLarge);
        }
        validate_page_record(record)?;
    }
    Ok(records)
}

#[derive(Clone, Debug, PartialEq)]
pub struct CatalogPageSnapshot {
    pub revision: u64,
    pub entries: Vec<CatalogParticipantEntry>,
    pub has_more: bool,
    pub next_cursor: Vec<u8>,
    pub cursor_expires_at_unix_seconds: i64,
}

#[derive(Debug)]
struct PendingEntry {
    revision: u64,
    participant_index: u32,
    participant_id: String,
    fragment_count: usize,
    total_bytes: usize,
    parts: Vec<Option<Vec<u8>>>,
}

/// One watch/room page assembler. The owner must retain only the last committed
/// snapshot and replace it after `finish_page`; incomplete entries stay private.
#[derive(Debug)]
pub struct CatalogAssembler {
    watch_id: u64,
    room: String,
    revision: u64,
    minimum_revision: u64,
    last_participant_index: Option<u32>,
    last_participant_id: Option<String>,
    pending: Option<PendingEntry>,
    entries: Vec<CatalogParticipantEntry>,
    page_bytes: usize,
    page_records: usize,
    page_complete: Option<(bool, Vec<u8>, i64)>,
    page_finished: bool,
    failed: bool,
    finished: bool,
}

impl CatalogAssembler {
    pub fn new(watch_id: u64, room: String, minimum_revision: u64) -> Result<Self, CatalogError> {
        if watch_id == 0 || !is_application_name(&room) {
            return Err(CatalogError::InvalidRequest);
        }
        Ok(Self {
            watch_id,
            room,
            revision: 0,
            minimum_revision,
            last_participant_index: None,
            last_participant_id: None,
            pending: None,
            entries: Vec::new(),
            page_bytes: 0,
            page_records: 0,
            page_complete: None,
            page_finished: false,
            failed: false,
            finished: false,
        })
    }

    /// Add one bounded record. A revision can contain many distinct entries;
    /// same-revision participant entries are expected and accepted in order.
    pub fn push_record(&mut self, record: CatalogPageRecord) -> Result<(), CatalogError> {
        if self.failed || self.finished {
            return Err(CatalogError::InvalidRecord);
        }
        let result = self.push_record_inner(record);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_record_inner(&mut self, record: CatalogPageRecord) -> Result<(), CatalogError> {
        validate_page_record(&record)?;
        if record.encoded_len() > MAX_CATALOG_RECORD_BYTES {
            return Err(CatalogError::RecordTooLarge);
        }
        if record.watch_id != self.watch_id || record.room != self.room {
            return Err(CatalogError::Inconsistent);
        }
        if record.revision < self.revision || record.revision < self.minimum_revision {
            return Err(CatalogError::StaleRevision);
        }
        if self.revision != 0 && record.revision != self.revision {
            return Err(CatalogError::Inconsistent);
        }
        let next_records = self.page_records.saturating_add(1);
        let next_bytes = self.page_bytes.saturating_add(record.encoded_len());
        if next_records > MAX_CATALOG_PAGE_RECORDS || next_bytes > MAX_CATALOG_PAGE_BYTES {
            return Err(CatalogError::PageTooLarge);
        }
        if self.page_finished {
            return Err(CatalogError::Inconsistent);
        }
        let marker = (
            record.has_more,
            record.next_cursor.clone(),
            record.cursor_expires_at_unix_seconds,
        );
        if self
            .page_complete
            .as_ref()
            .is_some_and(|prior| prior != &marker)
        {
            return Err(CatalogError::Inconsistent);
        }
        self.page_complete = Some(marker);
        self.page_finished = record.complete;
        self.revision = record.revision;
        self.page_records = next_records;
        self.page_bytes = next_bytes;
        for fragment in record.fragments {
            self.push_fragment(fragment)?;
        }
        if self.page_finished && self.pending.is_some() {
            return Err(CatalogError::Inconsistent);
        }
        Ok(())
    }

    fn push_fragment(&mut self, fragment: CatalogParticipantFragment) -> Result<(), CatalogError> {
        validate_fragment(&fragment)?;
        if fragment.watch_id != self.watch_id || fragment.revision != self.revision {
            return Err(CatalogError::Inconsistent);
        }
        if self.pending.is_none() {
            let expected_index = self
                .last_participant_index
                .map_or(0, |last| last.saturating_add(1));
            if fragment.participant_index != expected_index {
                return Err(
                    if self.last_participant_index == Some(fragment.participant_index) {
                        CatalogError::Duplicate
                    } else {
                        CatalogError::OutOfOrder
                    },
                );
            }
            if let Some(last_id) = self.last_participant_id.as_deref() {
                if fragment.participant_id == last_id {
                    return Err(CatalogError::Duplicate);
                }
                if fragment.participant_id.as_str() < last_id {
                    return Err(CatalogError::OutOfOrder);
                }
            }
            let count = fragment.fragment_count as usize;
            self.pending = Some(PendingEntry {
                revision: fragment.revision,
                participant_index: fragment.participant_index,
                participant_id: fragment.participant_id.clone(),
                fragment_count: count,
                total_bytes: 0,
                parts: vec![None; count],
            });
        }
        let pending = self.pending.as_mut().ok_or(CatalogError::InvalidRecord)?;
        if pending.revision != fragment.revision
            || pending.participant_index != fragment.participant_index
            || pending.participant_id != fragment.participant_id
            || pending.fragment_count != fragment.fragment_count as usize
        {
            return Err(CatalogError::Inconsistent);
        }
        let index = fragment.fragment_index as usize;
        if pending.parts.get(index).is_none() {
            return Err(CatalogError::InvalidRecord);
        }
        if pending.parts.get(index).is_some_and(Option::is_some) {
            return Err(CatalogError::Duplicate);
        }
        let total = pending
            .total_bytes
            .saturating_add(fragment.entry_chunk.len());
        if total > MAX_CATALOG_ENTRY_BYTES {
            return Err(CatalogError::EntryTooLarge);
        }
        pending.total_bytes = total;
        let Some(part) = pending.parts.get_mut(index) else {
            return Err(CatalogError::InvalidRecord);
        };
        *part = Some(fragment.entry_chunk);
        if pending.parts.iter().any(Option::is_none) {
            return Ok(());
        }
        let pending = self.pending.take().ok_or(CatalogError::InvalidRecord)?;
        let mut bytes = Vec::with_capacity(pending.total_bytes);
        for part in pending.parts {
            bytes.extend_from_slice(part.as_deref().ok_or(CatalogError::InvalidRecord)?);
        }
        let entry = CatalogParticipantEntry::decode(bytes.as_slice())
            .map_err(|_| CatalogError::InvalidEntry)?;
        let participant = entry
            .participant
            .as_ref()
            .ok_or(CatalogError::InvalidEntry)?;
        if participant.room_id != self.room
            || participant.participant_id != pending.participant_id
            || crate::state::validation::validate_participant(participant).is_err()
            || entry.tracks.len() > MAX_TRACKS_PER_PARTICIPANT
            || entry.tracks.iter().any(|track| {
                track.participant_id != participant.participant_id
                    || track.room_id != participant.room_id
                    || crate::state::validation::validate_track(track).is_err()
            })
        {
            return Err(if entry.tracks.len() > MAX_TRACKS_PER_PARTICIPANT {
                CatalogError::TooManyTracks
            } else {
                CatalogError::InvalidEntry
            });
        }
        if let Some(last_id) = self.last_participant_id.as_deref() {
            if pending.participant_id == last_id {
                return Err(CatalogError::Duplicate);
            }
            if pending.participant_id.as_str() < last_id {
                return Err(CatalogError::OutOfOrder);
            }
        }
        self.last_participant_id = Some(pending.participant_id);
        self.entries.push(entry);
        self.last_participant_index = Some(pending.participant_index);
        if self.entries.len() > MAX_CATALOG_PAGE_SIZE as usize {
            return Err(CatalogError::PageTooLarge);
        }
        Ok(())
    }

    /// Return the newly complete page only when it has no unfinished entry and
    /// the page record marked it complete. On failure, prior caller state is untouched.
    pub fn finish_page(&mut self) -> Result<Option<CatalogPageSnapshot>, CatalogError> {
        if self.failed || self.finished || !self.page_finished || self.pending.is_some() {
            return Ok(None);
        }
        let (has_more, next_cursor, cursor_expires_at_unix_seconds) =
            self.page_complete
                .as_ref()
                .ok_or(CatalogError::InvalidRecord)?;
        self.finished = true;
        Ok(Some(CatalogPageSnapshot {
            revision: self.revision,
            entries: core::mem::take(&mut self.entries),
            has_more: *has_more,
            next_cursor: next_cursor.clone(),
            cursor_expires_at_unix_seconds: *cursor_expires_at_unix_seconds,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DataDelivery, Track, track};
    use alloc::format;

    fn entry(id: &str, tracks: usize) -> CatalogParticipantEntry {
        CatalogParticipantEntry {
            participant: Some(crate::state::Participant {
                participant_id: id.into(),
                room_id: "room".into(),
            }),
            tracks: (0..tracks)
                .map(|i| Track {
                    publication_id: format!("pub_00000000001R0100000000{i:04}0"),
                    participant_id: id.into(),
                    room_id: "room".into(),
                    track_id: format!("track-{i:030}"),
                    kind: Some(track::Kind::Data(crate::state::DataTrack {
                        delivery: DataDelivery::Ordered as i32,
                    })),
                })
                .collect(),
        }
    }

    fn records(
        revision: u64,
        index: u32,
        value: &CatalogParticipantEntry,
    ) -> Vec<CatalogPageRecord> {
        fragment_entry(
            &CatalogPageMetadata {
                watch_id: 7,
                room: "room".into(),
                revision,
                complete: false,
                has_more: false,
                next_cursor: Vec::new(),
                cursor_expires_at_unix_seconds: 0,
            },
            index,
            value,
        )
        .expect("entry fits bounded records")
    }

    fn expected_page(revision: u64, entries: Vec<CatalogParticipantEntry>) -> CatalogPageSnapshot {
        CatalogPageSnapshot {
            revision,
            entries,
            has_more: false,
            next_cursor: Vec::new(),
            cursor_expires_at_unix_seconds: 0,
        }
    }

    fn completed_records(
        revision: u64,
        index: u32,
        value: &CatalogParticipantEntry,
    ) -> Vec<CatalogPageRecord> {
        let mut records = records(revision, index, value);
        records
            .last_mut()
            .expect("every participant entry has encoded bytes")
            .complete = true;
        records
    }

    #[test]
    fn exact_address_and_scoped_page_defaults_are_checked() {
        assert_eq!(
            validate_request(1, "room", 0, &[]),
            Ok(DEFAULT_CATALOG_PAGE_SIZE)
        );
        assert_eq!(
            validate_request(1, "room", 501, &[]),
            Err(CatalogError::InvalidRequest)
        );
        let mut address = LogicalSourceAddress {
            room: "room".into(),
            participant_id: "App_User".into(),
            kind: 2,
            track_id: "camera".into(),
        };
        assert_eq!(validate_address(&address), Ok(()));
        for participant in ["p".repeat(36), "P".into(), "_".into()] {
            address.participant_id = participant;
            assert_eq!(validate_address(&address), Ok(()));
        }
        for participant in [
            String::new(),
            "p".repeat(37),
            "é".into(),
            "app/user".into(),
            "p\n".into(),
            "p\u{0085}".into(),
        ] {
            address.participant_id = participant;
            assert_eq!(
                validate_address(&address),
                Err(CatalogError::InvalidAddress)
            );
        }
        address.participant_id = "App_User".into();
        address.kind = 0;
        assert_eq!(
            validate_address(&address),
            Err(CatalogError::InvalidAddress)
        );
    }

    #[test]
    fn sixty_four_canonical_track_names_fragment_under_full_protobuf_record_bound() {
        let value = entry("p", MAX_TRACKS_PER_PARTICIPANT);
        let fragments = completed_records(9, 0, &value);
        assert!(value.encoded_len() > MAX_CATALOG_RECORD_BYTES);
        assert!(fragments.len() > 1);
        assert!(fragments.len() <= MAX_CATALOG_ENTRY_FRAGMENTS);
        for record in &fragments {
            let encoded = encode_page_record(record).expect("framed record under byte limit");
            assert!(encoded.len() <= MAX_CATALOG_RECORD_BYTES);
            assert!(record.fragments.len() <= MAX_CATALOG_RECORDS);
            assert_eq!(decode_page_record(&encoded), Ok(record.clone()));
        }
        let mut assembler = CatalogAssembler::new(7, "room".into(), 0).expect("scope");
        for record in fragments.iter().take(fragments.len().saturating_sub(1)) {
            assembler
                .push_record(record.clone())
                .expect("valid incomplete fragment");
            assert_eq!(assembler.finish_page(), Ok(None));
        }
        assembler
            .push_record(fragments.last().expect("entry has a fragment").clone())
            .expect("final bounded fragment");
        assert_eq!(
            assembler.finish_page(),
            Ok(Some(expected_page(9, vec![value])))
        );
    }

    #[test]
    fn fragment_entry_rejects_counts_and_metadata_before_encoding() {
        let value = entry("p", MAX_TRACKS_PER_PARTICIPANT + 1);
        let metadata = CatalogPageMetadata {
            watch_id: 7,
            room: "room".into(),
            revision: 1,
            complete: false,
            has_more: false,
            next_cursor: Vec::new(),
            cursor_expires_at_unix_seconds: 0,
        };
        assert_eq!(
            fragment_entry(&metadata, 0, &value),
            Err(CatalogError::TooManyTracks)
        );

        let value = entry("p", 0);
        let mut metadata = CatalogPageMetadata {
            watch_id: 7,
            room: "room".into(),
            revision: 1,
            complete: false,
            has_more: false,
            next_cursor: Vec::new(),
            cursor_expires_at_unix_seconds: 0,
        };
        metadata.room = "r".repeat(257);
        assert_eq!(
            fragment_entry(&metadata, 0, &value),
            Err(CatalogError::InvalidRecord)
        );
        metadata.room = "room".into();
        metadata.next_cursor = vec![0; MAX_CATALOG_CURSOR_BYTES + 1];
        metadata.has_more = true;
        metadata.cursor_expires_at_unix_seconds = 1;
        assert_eq!(
            fragment_entry(&metadata, 0, &value),
            Err(CatalogError::InvalidRecord)
        );
    }

    #[test]
    fn zero_track_presence_and_two_entries_at_one_revision_assemble() {
        let first = entry("p1", 0);
        let second = entry("p2", 1);
        let mut assembler = CatalogAssembler::new(7, "room".into(), 0).expect("scope");
        for record in records(11, 0, &first) {
            assembler.push_record(record).expect("presence record");
            assert_eq!(assembler.finish_page(), Ok(None));
        }
        for record in completed_records(11, 1, &second) {
            assembler.push_record(record).expect("same page revision");
        }
        assert_eq!(
            assembler.finish_page(),
            Ok(Some(expected_page(11, vec![first, second])))
        );
    }

    #[test]
    fn participant_ids_must_be_strictly_ascending_and_page_stays_private() {
        for (second_id, expected) in [
            ("m", CatalogError::Duplicate),
            ("a", CatalogError::OutOfOrder),
        ] {
            let mut assembler = CatalogAssembler::new(7, "room".into(), 0).expect("scope");
            for record in records(11, 0, &entry("m", 0)) {
                assembler.push_record(record).expect("first complete entry");
            }
            let second = completed_records(11, 1, &entry(second_id, 0))
                .into_iter()
                .next()
                .expect("entry has a fragment");
            assert_eq!(assembler.push_record(second), Err(expected.clone()));
            assert_eq!(assembler.finish_page(), Ok(None));
        }
    }

    #[test]
    fn completed_smaller_page_retains_expiring_continuation() {
        let value = entry("p", 0);
        let mut record = completed_records(12, 0, &value)
            .pop()
            .expect("presence record");
        record.has_more = true;
        record.next_cursor = vec![4, 2, 1];
        record.cursor_expires_at_unix_seconds = 1234;
        let mut assembler = CatalogAssembler::new(7, "room".into(), 12).expect("scope");
        assembler.push_record(record).expect("bounded continuation");
        assert_eq!(
            assembler.finish_page(),
            Ok(Some(CatalogPageSnapshot {
                revision: 12,
                entries: vec![value],
                has_more: true,
                next_cursor: vec![4, 2, 1],
                cursor_expires_at_unix_seconds: 1234,
            }))
        );
    }

    #[test]
    fn rejects_stale_duplicate_inconsistent_and_oversized_metadata() {
        let mut pieces = completed_records(4, 0, &entry("p", 64));
        assert!(pieces.len() > 1);
        let first = pieces.remove(0);
        let mut duplicate = CatalogAssembler::new(7, "room".into(), 0).expect("scope");
        duplicate
            .push_record(first.clone())
            .expect("first fragment");
        assert_eq!(
            duplicate.push_record(first.clone()),
            Err(CatalogError::Duplicate)
        );
        assert_eq!(duplicate.finish_page(), Ok(None));

        let mut wrong_scope = (*pieces
            .first()
            .ok_or(CatalogError::InvalidRecord)
            .expect("fragment remains after the first"))
        .clone();
        wrong_scope.room = "else".into();
        let mut inconsistent = CatalogAssembler::new(7, "room".into(), 0).expect("scope");
        inconsistent
            .push_record(first.clone())
            .expect("first fragment");
        assert_eq!(
            inconsistent.push_record(wrong_scope),
            Err(CatalogError::Inconsistent)
        );
        assert_eq!(inconsistent.finish_page(), Ok(None));

        let mut assembler = CatalogAssembler::new(7, "room".into(), 0).expect("scope");
        assembler.push_record(first).expect("first fragment");
        for piece in pieces {
            assembler.push_record(piece).expect("remaining fragment");
        }
        assert!(assembler.finish_page().expect("complete page").is_some());
        let stale = records(3, 1, &entry("p2", 0)).remove(0);
        assert_eq!(
            assembler.push_record(stale),
            Err(CatalogError::InvalidRecord)
        );

        let mut oversized = records(5, 0, &entry("p", 0)).remove(0);
        oversized.next_cursor = vec![0; MAX_CATALOG_CURSOR_BYTES + 1];
        oversized.has_more = true;
        oversized.cursor_expires_at_unix_seconds = 1;
        assert_eq!(
            encode_page_record(&oversized),
            Err(CatalogError::InvalidRecord)
        );
        assert_eq!(
            decode_page_record(&vec![0; MAX_CATALOG_RECORD_BYTES + 1]),
            Err(CatalogError::RecordTooLarge)
        );

        let mut old_page = CatalogAssembler::new(7, "room".into(), 5).expect("revision floor");
        let old_record = records(4, 0, &entry("p", 0)).remove(0);
        assert_eq!(
            old_page.push_record(old_record),
            Err(CatalogError::StaleRevision)
        );

        let mut out_of_order = CatalogAssembler::new(7, "room".into(), 0).expect("scope");
        assert_eq!(
            out_of_order.push_record(records(6, 1, &entry("p2", 0)).remove(0)),
            Err(CatalogError::OutOfOrder)
        );
    }
}
