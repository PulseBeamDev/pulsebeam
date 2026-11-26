use super::*;
use crate::ffi::{ErrorCode, ErrorInfo};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Publication {
    source: TrackSelector,
    id: String,
}

#[derive(Default)]
struct Lane {
    messages: VecDeque<(usize, DataEvent)>,
    bytes: usize,
    failure: Option<DataEvent>,
}

#[derive(Default)]
pub(super) struct DataSetQueue {
    observed: BTreeMap<TrackSelector, (String, TopicMode)>,
    lanes: BTreeMap<Publication, Lane>,
    ready: VecDeque<Publication>,
    failed: BTreeSet<Publication>,
    messages: usize,
    bytes: usize,
}

impl DataSetQueue {
    const LANE_BYTES: usize = 128 * 1024;
    const LANE_MESSAGES: usize = 32;
    const FAILED_PUBLICATIONS: usize = DATA_RECEIVE_CAPACITY;

    pub(super) fn push(
        &mut self,
        source: &TrackSelector,
        publication: &str,
        mode: TopicMode,
        event: DataEvent,
    ) -> Result<bool, ErrorInfo> {
        let key = Publication {
            source: source.clone(),
            id: publication.into(),
        };
        if self.failed.contains(&key) {
            return Ok(false);
        }
        let retired = self.retire(source, Some(publication))?;
        if !self.observed.contains_key(source) {
            if self.observed.len() == DATA_RECEIVE_CAPACITY {
                return Err(ErrorInfo::new(
                    ErrorCode::CapacityExceeded,
                    "Data set source capacity exceeded",
                ));
            }
            self.observed
                .insert(source.clone(), (publication.into(), mode));
        }
        let bytes = event.retained_bytes();
        let lane = self.lanes.entry(key.clone()).or_default();
        let was_ready = !lane.messages.is_empty();
        if mode == TopicMode::Latest {
            self.messages -= lane.messages.len();
            self.bytes -= lane.bytes;
            lane.messages.clear();
            lane.bytes = 0;
        }
        let full = |lane: &Lane, messages, queued_bytes| {
            lane.messages.len() == Self::LANE_MESSAGES
                || lane.bytes + bytes > Self::LANE_BYTES
                || messages == DATA_RECEIVE_CAPACITY
                || queued_bytes + bytes > DataReadQueue::BYTE_LIMIT
        };
        while full(lane, self.messages, self.bytes) {
            if mode == TopicMode::Lossy
                && let Some((removed, _)) = lane.messages.pop_front()
            {
                lane.bytes -= removed;
                self.bytes -= removed;
                self.messages -= 1;
                continue;
            }
            if lane.messages.is_empty() {
                self.lanes.remove(&key);
                if was_ready {
                    self.ready.retain(|ready| ready != &key);
                }
            }
            return if mode == TopicMode::Ordered {
                self.fail(
                    key,
                    Self::continuity_loss("Reliable data set buffer was exhausted"),
                )
            } else {
                Ok(retired)
            };
        }
        lane.messages.push_back((bytes, event));
        lane.bytes += bytes;
        self.messages += 1;
        self.bytes += bytes;
        if !was_ready {
            self.ready.push_back(key);
        }
        Ok(true)
    }

    pub(super) fn retire(
        &mut self,
        source: &TrackSelector,
        replacement: Option<&str>,
    ) -> Result<bool, ErrorInfo> {
        if !self
            .observed
            .get(source)
            .is_some_and(|(current, _)| Some(current.as_str()) != replacement)
        {
            return Ok(false);
        }
        let Some((id, TopicMode::Ordered)) = self.observed.remove(source) else {
            return Ok(false);
        };
        self.fail(
            Publication {
                source: source.clone(),
                id,
            },
            Self::continuity_loss("Reliable source was retired"),
        )
    }

    pub(super) fn revoke_unmatched(
        &mut self,
        mut matches: impl FnMut(&TrackSelector) -> bool,
    ) -> Result<bool, ErrorInfo> {
        let retired: Vec<_> = self
            .observed
            .keys()
            .filter(|source| !matches(source))
            .cloned()
            .collect();
        let mut ready = false;
        for source in retired {
            ready |= self.retire(&source, None)?;
        }
        Ok(ready)
    }

    fn fail(&mut self, key: Publication, error: ErrorInfo) -> Result<bool, ErrorInfo> {
        if self.failed.contains(&key) {
            return Ok(false);
        }
        if self.failed.len() == Self::FAILED_PUBLICATIONS {
            return Err(Self::continuity_loss(
                "Data set could not retain another source failure",
            ));
        }
        self.failed.insert(key.clone());
        let lane = self.lanes.entry(key.clone()).or_default();
        lane.failure = Some(DataEvent::SourceFailed {
            participant_id: key.source.participant_id.clone(),
            track_id: key.source.track_id.clone(),
            publication_id: key.id.clone(),
            delivery: crate::ffi::DataDelivery::Reliable,
            error,
        });
        if lane.messages.is_empty() {
            self.ready.push_back(key);
        }
        Ok(true)
    }

    pub(super) fn pop(&mut self) -> Option<DataEvent> {
        let key = self.ready.pop_front()?;
        let lane = self.lanes.get_mut(&key).expect("queued data lane");
        let event = if let Some((bytes, event)) = lane.messages.pop_front() {
            lane.bytes -= bytes;
            self.bytes -= bytes;
            self.messages -= 1;
            Some(event)
        } else {
            lane.failure.take()
        };
        if lane.messages.is_empty() && lane.failure.is_none() {
            self.lanes.remove(&key);
        } else {
            self.ready.push_back(key);
        }
        event
    }

    fn continuity_loss(message: &str) -> ErrorInfo {
        ErrorInfo::new(ErrorCode::ContinuityLost, message)
    }
}
