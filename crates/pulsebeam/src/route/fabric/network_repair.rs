mod media;

pub(crate) use media::{MediaGaps, MediaInterest};

use std::{collections::BTreeMap, num::NonZeroU64};

use pulsebeam_core::fabric::{Profile, wan::Window};
use pulsebeam_link::FabricIngress;
use pulsebeam_runtime::time::{Duration, Instant};

use super::local::{Emission, RepairFragment, RepairRequest};
use crate::entity::{PublicationKey, TrackKind};

const PENDING_LIMIT: usize = 32;
pub(crate) const MAX_REPAIR_LIFETIME: Duration = Duration::from_secs(2);

pub(crate) const fn supports_media_refresh(profile: Profile) -> bool {
    matches!(profile, Profile::H264 | Profile::Vp9)
}

/// Repair authority is separate from live relay authority: it names one
/// authenticated ingress, source incarnation, and destination branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RepairRoute {
    pub(crate) ingress: FabricIngress,
    pub(crate) publication: PublicationKey,
    pub(crate) source: NonZeroU64,
    pub(crate) branch: NonZeroU64,
    pub(crate) major: u8,
    pub(crate) revision: u8,
    pub(crate) emission: Emission,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Demand {
    pub(crate) request: RepairRequest,
    pub(crate) sequence: u64,
    pub(crate) expires: Instant,
}

impl Demand {
    pub(crate) fn wire_sequence(self) -> u32 {
        let [a, b, c, d, ..] = self.sequence.to_le_bytes();
        u32::from_le_bytes([a, b, c, d])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Interest {
    pub(crate) parent: super::LiveNetworkChild,
    pub(crate) receiver: crate::entity::ConnectionId,
    pub(crate) branch: NonZeroU64,
    pub(crate) request: RepairRequest,
    pub(crate) expires: Instant,
}

#[derive(Debug)]
pub(crate) enum InterestEvent {
    Request(Interest),
    Cancel(Interest),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RepairChild {
    pub(crate) parent: FabricIngress,
    pub(crate) route: RepairRoute,
    pub(crate) destination: crate::route::NodeRouteAddress,
    pub(crate) receiver: crate::entity::ConnectionId,
    pub(crate) demand: Demand,
    pub(crate) window: Option<Window>,
}

impl RepairChild {
    pub(crate) fn matches_parent(self, parent: super::LiveNetworkChild) -> bool {
        let route = self.route;
        parent.ingress == self.parent
            && parent.destination == self.destination
            && parent.publication == route.publication
            && parent.incarnation == route.source
            && parent.emission == route.emission
            && parent.major == route.major
            && parent.revision == route.revision
            && parent.ingress.association == route.ingress.association
            && parent.ingress.key != route.ingress.key
            && parent.ingress.purpose == pulsebeam_link::FabricPurpose::Live
            && route.ingress.purpose == pulsebeam_link::FabricPurpose::RepairOnly
            && match (parent.window, self.window) {
                (None, None) => true,
                (Some(live), Some(repair)) => {
                    repair.ssrc().get().to_be_bytes() == route.ingress.key.to_bytes()
                        && live.repair_window(self.demand.sequence, repair.ssrc()).ok()
                            == Some(repair)
                }
                _ => false,
            }
    }

    pub(crate) fn accept_image(
        self,
        image: super::NetworkPacket,
    ) -> Option<super::local::OwnedPacket> {
        (self.window == Some(image.window) && image.position == self.demand.sequence)
            .then_some(image.packet)
    }
}

/// A WAN-only receive ticket for one exact RTP source position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MediaRepairChild {
    pub(crate) parent: super::LiveNetworkChild,
    pub(crate) window: Window,
    pub(crate) position: u64,
    pub(crate) expires: Instant,
}

impl MediaRepairChild {
    pub(crate) fn valid(self) -> bool {
        self.parent.ingress.purpose == pulsebeam_link::FabricPurpose::Live
            && self.parent.window.is_some_and(|live| {
                live.profile() != Profile::Data
                    && live.contains(self.position)
                    && live.repair_window(self.position, self.window.ssrc()).ok()
                        == Some(self.window)
            })
    }

    pub(crate) fn ingress(self) -> FabricIngress {
        FabricIngress {
            association: self.parent.ingress.association,
            key: pulsebeam_link::NodeRouteKey::from_bytes(self.window.ssrc().get().to_be_bytes()),
            purpose: pulsebeam_link::FabricPurpose::RepairOnly,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    Installed,
    Replay,
    Refused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Pending,
    Acknowledged,
    Claimed,
}

struct Entry {
    demand: Demand,
    phase: Phase,
}

pub(crate) struct RepairIngress {
    route: RepairRoute,
    expires: Instant,
    last: Option<NonZeroU64>,
    entries: BTreeMap<RepairFragment, Entry>,
}

impl RepairIngress {
    pub(crate) fn new(route: RepairRoute, expires: Instant) -> Option<Self> {
        (route.publication.kind() == TrackKind::Data
            && route.ingress.purpose == pulsebeam_link::FabricPurpose::RepairOnly
            && route.ingress.key != pulsebeam_link::NodeRouteKey::from_bytes([0; 4])
            && route.emission.profile() == Profile::Data
            && route.emission.fits_registry(route.major, route.revision))
        .then(|| Self {
            route,
            expires,
            last: None,
            entries: BTreeMap::new(),
        })
    }

    pub(crate) fn install(
        &mut self,
        route: RepairRoute,
        demand: Demand,
        now: Instant,
    ) -> Admission {
        if route != self.route
            || now >= self.expires
            || now >= demand.expires
            || demand.expires > self.expires
            || demand.request.fragment.index >= pulsebeam_proto::data_record::MAX_FRAGMENTS
        {
            return Admission::Refused;
        }
        self.entries.retain(|_, entry| now < entry.demand.expires);
        let fragment = demand.request.fragment;
        if self
            .entries
            .get(&fragment)
            .is_some_and(|entry| entry.demand == demand)
        {
            return Admission::Replay;
        }
        if self.last.is_some_and(|last| demand.request.id <= last) {
            return Admission::Refused;
        }
        self.last = Some(demand.request.id);
        if self.entries.contains_key(&fragment) || self.entries.len() >= PENDING_LIMIT {
            return Admission::Refused;
        }
        self.entries.insert(
            fragment,
            Entry {
                demand,
                phase: Phase::Pending,
            },
        );
        Admission::Installed
    }

    fn current(&mut self, route: RepairRoute, demand: Demand, now: Instant) -> Option<&mut Entry> {
        if route != self.route || now >= self.expires || now >= demand.expires {
            return None;
        }
        self.entries
            .get_mut(&demand.request.fragment)
            .filter(|entry| entry.demand == demand)
    }

    pub(crate) fn acknowledge(&mut self, route: RepairRoute, demand: Demand, now: Instant) -> bool {
        let Some(entry) = self.current(route, demand, now) else {
            return false;
        };
        if entry.phase == Phase::Pending {
            entry.phase = Phase::Acknowledged;
        }
        true
    }

    /// The caller supplies the identity of a validated DATA record, not its
    /// wire SSRC or a unit inferred from ordering. Only this route's exact
    /// pending demand can claim the packet.
    pub(crate) fn claim(
        &mut self,
        ingress: FabricIngress,
        fragment: RepairFragment,
        sequence: u64,
        now: Instant,
    ) -> Option<Demand> {
        let entry = self.entries.get_mut(&fragment)?;
        if ingress != self.route.ingress
            || now >= entry.demand.expires
            || now >= self.expires
            || entry.demand.sequence != sequence
            || entry.phase != Phase::Acknowledged
        {
            return None;
        }
        entry.phase = Phase::Claimed;
        Some(entry.demand)
    }

    pub(crate) fn release(&mut self, route: RepairRoute, demand: Demand, now: Instant) -> bool {
        let Some(entry) = self
            .current(route, demand, now)
            .filter(|entry| entry.phase == Phase::Claimed)
        else {
            return false;
        };
        entry.phase = Phase::Acknowledged;
        true
    }

    pub(crate) fn handoff(
        &mut self,
        route: RepairRoute,
        demand: Demand,
        now: Instant,
        write: impl FnOnce() -> bool,
    ) -> bool {
        if self
            .current(route, demand, now)
            .is_none_or(|entry| entry.phase != Phase::Claimed)
            || !write()
        {
            return false;
        }
        self.entries.remove(&demand.request.fragment);
        true
    }

    pub(crate) fn cancel(&mut self, route: RepairRoute, demand: Demand, now: Instant) -> bool {
        if self.current(route, demand, now).is_none() {
            return false;
        }
        self.entries.remove(&demand.request.fragment);
        true
    }
}

#[cfg(test)]
mod tests;
