use std::{collections::VecDeque, mem::size_of, sync::Arc};

use bytes::Bytes;

use super::{IngressOwner, QueuedEvent};
use crate::{Event, RtpExtension, packet::RtpPacket};

pub(super) fn owned_bytes(event: &Event) -> Option<usize> {
    match event {
        Event::EncodingDiscovered(info) => info
            .mid
            .len()
            .checked_add(info.rid.as_deref().map_or(0, str::len))?
            .checked_add(std::mem::size_of_val(info.payloads.as_ref()))?
            .checked_add(info.payloads.iter().try_fold(0usize, |bytes, payload| {
                bytes.checked_add(payload.name.len())
            })?),
        Event::Rtp { mid, rid, packet } => mid
            .len()
            .checked_add(rid.as_deref().map_or(0, str::len))?
            .checked_add(
                packet
                    .extensions
                    .capacity()
                    .checked_mul(size_of::<RtpExtension>())?,
            )?
            .checked_add(
                packet
                    .extensions
                    .iter()
                    .try_fold(0usize, |bytes, extension| {
                        bytes.checked_add(extension.uri.len())
                    })?,
            ),
        _ => Some(0),
    }
}

impl QueuedEvent {
    pub(super) fn new(event: Event, retained_bytes: usize) -> Option<Self> {
        Some(Self {
            owned_bytes: owned_bytes(&event)?,
            event,
            retained_bytes,
        })
    }
}

pub(super) struct Reservation {
    backing: crate::event_buffer::Reservation<QueuedEvent>,
    pub(super) runtime_bytes: usize,
}

impl Reservation {
    pub(super) fn commit(self, queue: &mut VecDeque<QueuedEvent>) -> usize {
        self.backing.commit(queue);
        self.runtime_bytes
    }
}

impl IngressOwner {
    pub(super) fn prepare_extensions(
        &self,
        media: u64,
        packet: &RtpPacket<'_>,
        bytes: &Bytes,
        strings: usize,
    ) -> Option<Vec<RtpExtension>> {
        let ranges = || {
            packet
                .extensions()
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|value| {
                    self.media
                        .get(&media)?
                        .extensions
                        .iter()
                        .find(|(id, _)| *id == value.id())
                        .map(|(_, uri)| (uri.as_ref(), value.range()))
                })
        };
        let (count, strings) =
            ranges().try_fold((0usize, strings), |(count, strings), (uri, _)| {
                Some((count.checked_add(1)?, strings.checked_add(uri.len())?))
            })?;
        let fits = |capacity: usize| {
            strings
                .checked_add(capacity.checked_mul(size_of::<RtpExtension>())?)?
                .checked_add(self.runtime_bytes)
                .filter(|total| *total <= self.max_runtime_bytes)
        };
        fits(count)?;
        let mut extensions = Vec::new();
        extensions.try_reserve_exact(count).ok()?;
        fits(extensions.capacity())?;
        extensions.extend(ranges().map(|(uri, range)| RtpExtension {
            uri: Arc::from(uri),
            value: bytes.slice(range),
        }));
        Some(extensions)
    }

    pub(super) fn event_backing_bytes(&self) -> usize {
        self.events.capacity() * size_of::<QueuedEvent>()
    }

    pub(super) fn prepare_events(
        &self,
        runtime_bytes: usize,
        future: usize,
        additional: usize,
    ) -> Option<Reservation> {
        let base = runtime_bytes.checked_sub(self.event_backing_bytes())?;
        let required = self
            .events
            .len()
            .checked_add(future)?
            .checked_add(additional)?;
        let backing = crate::event_buffer::Reservation::prepare(
            &self.events,
            required,
            self.max_runtime_bytes.checked_sub(base)?,
        )?;
        Some(Reservation {
            runtime_bytes: base.checked_add(backing.bytes())?,
            backing,
        })
    }

    pub(super) fn push_event(&mut self, event: QueuedEvent) {
        assert!(
            self.events.len()
                < self
                    .events
                    .capacity()
                    .saturating_sub(self.runtime_reservations.len())
        );
        self.events.push_back(event);
    }

    pub(super) fn release_idle_events(&mut self) {
        if self.events.is_empty() && self.runtime_reservations.is_empty() {
            self.runtime_bytes = self
                .runtime_bytes
                .saturating_sub(self.event_backing_bytes());
            self.events = VecDeque::new();
        }
    }
}
