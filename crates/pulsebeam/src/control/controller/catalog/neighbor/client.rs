//! Bounded viewer-side read windows. The association owns request IDs, never
//! the tenant's credential. Callers recheck the current watch and declaration
//! before sending, receiving, or using a window.

use super::super::{
    CurrentFuture, ParticipantId, REGIONAL_WATCHES, WatchKey, owner,
    responders::{DeclarationId, ReadScope, ReadStep, RoomRead},
    wire,
};
use pulsebeam_runtime::time::Instant;
use std::{collections::BTreeMap, num::NonZeroU64, time::Duration};

const REQUEST_TTL: Duration = Duration::from_secs(1);
const WINDOW_TTL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::control::controller) struct Demand {
    pub(in crate::control::controller) watch: CurrentFuture,
    pub(in crate::control::controller) round: NonZeroU64,
    pub(in crate::control::controller) after: Option<ParticipantId>,
    pub(in crate::control::controller) revision: Option<owner::Revision>,
}

impl Demand {
    fn key(self) -> WatchKey {
        (self.watch.scope.viewer, self.watch.watch_id)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::control::controller) struct Order {
    pub(in crate::control::controller) demand: Demand,
    pub(in crate::control::controller) declaration: DeclarationId,
}

impl Order {
    fn same_read(self, other: Self) -> bool {
        self.declaration == other.declaration
            && self.demand.watch == other.demand.watch
            && self.demand.round == other.demand.round
    }
}

struct Flight {
    id: NonZeroU64,
    order: Order,
    after: Option<ParticipantId>,
    floor: Option<ParticipantId>,
    revision: Option<owner::Revision>,
    expires: Instant,
}

pub(in crate::control::controller) struct Window {
    order: Order,
    after: Option<ParticipantId>,
    expires: Instant,
    result: owner::NeighborWindow,
}

impl Window {
    fn matches(&self, order: Order) -> bool {
        self.order.same_read(order)
            && order
                .demand
                .revision
                .is_none_or(|revision| revision == self.revision())
    }

    fn floor(&self) -> Option<&str> {
        self.after.as_ref().map(ParticipantId::as_str)
    }

    fn refill_after(&self, order: Order) -> Option<ParticipantId> {
        let after = order.demand.after;
        if let owner::NeighborWindow::ScanMore {
            scanned_through, ..
        } = &self.result
            && after.as_ref().map(ParticipantId::as_str) >= self.floor()
            && after.as_ref().map(ParticipantId::as_str) < Some(scanned_through.as_str())
        {
            return ParticipantId::new(scanned_through).ok();
        }
        after
    }

    pub(in crate::control::controller) fn fresh_until(&self) -> Instant {
        self.expires
    }
}

impl RoomRead for Window {
    fn scope(&self) -> ReadScope {
        self.order.demand.watch.scope
    }

    fn revision(&self) -> owner::Revision {
        match self.result {
            owner::NeighborWindow::Row { revision, .. }
            | owner::NeighborWindow::ScanMore { revision, .. }
            | owner::NeighborWindow::Exhausted { revision } => revision,
        }
    }

    fn next(&self, after: Option<&str>) -> Result<ReadStep, wire::FailureCode> {
        if after < self.floor() {
            return Ok(ReadStep::Refill);
        }
        match &self.result {
            owner::NeighborWindow::Exhausted { .. } => Ok(ReadStep::Exhausted),
            owner::NeighborWindow::Row { candidate, .. }
                if after < Some(candidate.canonical.as_str()) =>
            {
                Ok(ReadStep::Row(candidate.clone()))
            }
            _ => Ok(ReadStep::Refill),
        }
    }
}

#[derive(Default)]
pub(in crate::control::controller) struct Reads {
    windows: BTreeMap<WatchKey, Window>,
    flight: Option<Flight>,
    serial: u64,
    last_sent: Option<WatchKey>,
}

impl Reads {
    pub(in crate::control::controller) fn window(
        &self,
        order: Order,
        now: Instant,
    ) -> Option<&Window> {
        self.windows
            .get(&order.demand.key())
            .filter(|window| now < window.expires && window.matches(order))
    }

    pub(in crate::control::controller) fn prune(
        &mut self,
        desired: &[Order],
        now: Instant,
        mut invalidate: impl FnMut(CurrentFuture),
    ) {
        self.windows.retain(|_, window| {
            if now < window.expires && desired.iter().any(|order| window.matches(*order)) {
                return true;
            }
            invalidate(window.order.demand.watch);
            false
        });
        if let Some(flight) = &self.flight
            && (now >= flight.expires || !desired.contains(&flight.order))
        {
            if now >= flight.expires {
                self.windows.remove(&flight.order.demand.key());
                invalidate(flight.order.demand.watch);
            }
            self.flight = None;
        }
    }

    pub(in crate::control::controller) fn send(
        &mut self,
        desired: &[Order],
        now: Instant,
        mut send: impl FnMut(NonZeroU64, Order, Option<ParticipantId>, Option<owner::Revision>) -> bool,
    ) {
        if self.flight.is_some() {
            return;
        }
        let Some(id) = self.serial.checked_add(1).and_then(NonZeroU64::new) else {
            return;
        };
        let Some(expires) = now.checked_add(REQUEST_TTL) else {
            return;
        };
        let needs_read = |order: &Order| {
            self.window(*order, now).is_none_or(|window| {
                matches!(
                    window.next(order.demand.after.as_ref().map(ParticipantId::as_str)),
                    Ok(ReadStep::Refill)
                )
            })
        };
        let order = desired
            .iter()
            .filter(|order| needs_read(order))
            .min_by_key(|order| {
                let key = order.demand.key();
                (self.last_sent.is_some_and(|last| key <= last), key)
            })
            .copied();
        let Some(order) = order else { return };
        let key = order.demand.key();
        let window = self.window(order, now);
        let after = window.map_or(order.demand.after, |view| view.refill_after(order));
        let floor = if after != order.demand.after {
            // The prior scan proved there were no visible rows in this skipped range.
            window.and_then(|window| window.after)
        } else {
            after
        };
        let revision = window.map(RoomRead::revision).or(order.demand.revision);
        if send(id, order, after, revision) {
            self.serial = id.get();
            self.last_sent = Some(key);
            self.flight = Some(Flight {
                id,
                order,
                after,
                floor,
                revision,
                expires,
            });
        }
    }

    pub(in crate::control::controller) fn receive(
        &mut self,
        id: NonZeroU64,
        result: Result<owner::NeighborWindow, wire::FailureCode>,
        now: Instant,
        desired: &[Order],
    ) -> Option<(CurrentFuture, Result<(), wire::FailureCode>)> {
        if self.flight.as_ref()?.id != id {
            return None;
        }
        let flight = self.flight.take()?;
        if now >= flight.expires || !desired.contains(&flight.order) {
            return None;
        }
        let Flight {
            order,
            after,
            floor,
            revision,
            ..
        } = flight;
        let watch = order.demand.watch;
        let key = order.demand.key();
        let result = match result {
            Ok(result) => result,
            Err(code) => {
                self.windows.remove(&key);
                return Some((watch, Err(code)));
            }
        };
        let lower_bound = after.as_ref().map(ParticipantId::as_str);
        let valid = match &result {
            owner::NeighborWindow::Row { candidate, .. } => candidate
                .entry
                .as_ref()
                .and_then(|row| row.participant.as_ref())
                .is_some_and(|row| {
                    row.room_id == watch.scope.room.room.as_str()
                        && Some(row.participant_id.as_str()) > lower_bound
                }),
            owner::NeighborWindow::ScanMore {
                scanned_through, ..
            } => Some(scanned_through.as_str()) > lower_bound,
            owner::NeighborWindow::Exhausted { .. } => true,
        };
        let window = Window {
            order,
            after: floor,
            expires: now.checked_add(WINDOW_TTL)?,
            result,
        };
        if !valid || revision.is_some_and(|rev| rev != window.revision()) {
            self.windows.remove(&key);
            return Some((watch, Err(wire::FailureCode::ResourceGone)));
        }
        if self.windows.len() >= REGIONAL_WATCHES && !self.windows.contains_key(&key) {
            return Some((watch, Err(wire::FailureCode::CapacityExceeded)));
        }
        self.windows.insert(key, window);
        Some((watch, Ok(())))
    }
}

#[cfg(test)]
mod tests;
