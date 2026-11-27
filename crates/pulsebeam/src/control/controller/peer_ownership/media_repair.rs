mod receives;
mod sends;

pub(super) use receives::RepairReceives;
pub(in crate::control::controller) use sends::{Grant, Source};
pub(super) use sends::{Packet, Receipt, RepairRequests};

use std::num::{NonZeroU32, NonZeroU64};

use pulsebeam_core::fabric::{
    Profile,
    wan::{FEEDBACK_SSRC, WINDOW_TUPLE_BYTES, Window},
};

const DEMAND: [u8; 2] = [1, 0x0c];
const READY: [u8; 2] = [1, 0x0d];
const REFRESH: [u8; 2] = [1, 0x18];
const DEMAND_BYTES: usize = 38;
const READY_BYTES: usize = DEMAND_BYTES + WINDOW_TUPLE_BYTES;

/// Source-position repair on an authenticated live media branch, not endpoint RTX.
/// Decoding establishes wire identity only. The owner must authorize the demand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control::controller) struct Demand {
    pub(in crate::control::controller) live: NonZeroU32,
    pub(in crate::control::controller) source: NonZeroU64,
    pub(super) branch: NonZeroU64,
    pub(super) request: NonZeroU64,
    pub(super) position: u64,
}

impl Demand {
    fn prefix(bytes: &[u8]) -> Option<Self> {
        let live = NonZeroU32::new(u32::from_be_bytes(bytes.get(2..6)?.try_into().ok()?))?;
        if live.get() == FEEDBACK_SSRC {
            return None;
        }
        let long = |range: std::ops::Range<usize>| {
            Some(u64::from_be_bytes(bytes.get(range)?.try_into().ok()?))
        };
        Some(Self {
            live,
            source: NonZeroU64::new(long(6..14)?)?,
            branch: NonZeroU64::new(long(14..22)?)?,
            request: NonZeroU64::new(long(22..30)?)?,
            position: long(30..38)?,
        })
    }

    fn write(self, out: &mut Vec<u8>, kind: [u8; 2]) {
        out.extend(kind);
        out.extend(self.live.get().to_be_bytes());
        out.extend(self.source.get().to_be_bytes());
        out.extend(self.branch.get().to_be_bytes());
        out.extend(self.request.get().to_be_bytes());
        out.extend(self.position.to_be_bytes());
    }

    pub(super) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != DEMAND_BYTES || !bytes.starts_with(&DEMAND) {
            return None;
        }
        Self::prefix(bytes)
    }

    pub(super) fn encode(self) -> Option<Box<[u8]>> {
        if self.live.get() == FEEDBACK_SSRC {
            return None;
        }
        let mut bytes = Vec::with_capacity(DEMAND_BYTES);
        self.write(&mut bytes, DEMAND);
        Some(bytes.into_boxed_slice())
    }

    pub(in crate::control::controller) fn send_refresh(
        lane: &pulsebeam_link::PeerControl,
        epoch: pulsebeam_runtime::time::Instant,
        interest: crate::route::fabric::network_repair::MediaInterest,
        now: pulsebeam_runtime::time::Instant,
    ) -> bool {
        let Some(window) = interest
            .parent
            .window
            .filter(|window| window.contains(interest.position))
        else {
            return false;
        };
        if now >= interest.expires {
            return false;
        }
        let Some(deadline) = interest
            .expires
            .checked_duration_since(epoch)
            .and_then(|elapsed| u64::try_from(elapsed.as_nanos()).ok())
            .map(pulsebeam_link::LinkTime)
        else {
            return false;
        };
        let refresh = Self {
            live: window.ssrc(),
            source: interest.parent.incarnation,
            branch: interest.branch,
            request: interest.branch,
            position: interest.position,
        };
        let sent = lane.try_send(refresh.encode_refresh(), deadline).is_ok();
        tracing::debug!(?refresh, sent, "Sent WAN media refresh request");
        sent
    }

    fn encode_refresh(self) -> Box<[u8]> {
        let mut bytes = Vec::with_capacity(DEMAND_BYTES);
        self.write(&mut bytes, REFRESH);
        bytes.into_boxed_slice()
    }

    pub(in crate::control::controller) fn decode_refresh(bytes: &[u8]) -> Option<Self> {
        (bytes.len() == DEMAND_BYTES && bytes.starts_with(&REFRESH)).then(|| Self::prefix(bytes))?
    }

    pub(in crate::control::controller) fn matches_parent(self, parent: Window) -> bool {
        parent.profile() != Profile::Data
            && parent.ssrc() == self.live
            && parent.contains(self.position)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ready {
    pub(super) demand: Demand,
    pub(super) window: Window,
}

impl Ready {
    pub(super) fn send_permit(self) -> crate::control::link::wan::RepairSendPermit {
        crate::control::link::wan::RepairSendPermit {
            live: pulsebeam_link::NodeRouteKey::from_bytes(self.demand.live.get().to_be_bytes()),
            source: self.demand.source,
            position: self.demand.position,
            window: self.window,
        }
    }

    pub(super) fn read_demand(bytes: &[u8]) -> Option<Demand> {
        if bytes.len() != READY_BYTES || !bytes.starts_with(&READY) {
            return None;
        }
        Demand::prefix(bytes)
    }

    pub(super) fn decode(bytes: &[u8], parent: Window) -> Option<Self> {
        let demand = Self::read_demand(bytes)?;
        let window = Window::from_tuple(
            bytes.get(DEMAND_BYTES..)?.try_into().ok()?,
            parent.emission(),
        )
        .ok()?;
        let ready = Self { demand, window };
        ready.matches_parent(parent).then_some(ready)
    }

    fn matches_parent(self, parent: Window) -> bool {
        self.demand.matches_parent(parent)
            && parent
                .repair_window(self.demand.position, self.window.ssrc())
                .ok()
                == Some(self.window)
    }

    pub(super) fn encode(self, parent: Window) -> Option<Box<[u8]>> {
        if !self.matches_parent(parent) {
            return None;
        }
        let mut bytes = Vec::with_capacity(READY_BYTES);
        self.demand.write(&mut bytes, READY);
        bytes.extend(self.window.to_tuple());
        Some(bytes.into_boxed_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_media_repair_uses_the_approved_demand_and_ready_vectors() {
        let demand_wire = hex::decode(
            "010c05060708000000000000000100000000000000020000000000000004000000000001ffff",
        )
        .unwrap();
        let ready_wire = hex::decode("010d05060708000000000000000100000000000000020000000000000004000000000001ffff0301010200000000000180000305060709").unwrap();
        let demand = Demand {
            live: NonZeroU32::new(0x0506_0708).unwrap(),
            source: NonZeroU64::MIN,
            branch: NonZeroU64::new(2).unwrap(),
            request: NonZeroU64::new(4).unwrap(),
            position: 0x1ffff,
        };
        let parent = Window::new(Profile::H264, 0x18000, 3, demand.live, 4).unwrap();
        let ready = Ready {
            demand,
            window: parent
                .repair_window(demand.position, NonZeroU32::new(0x0506_0709).unwrap())
                .unwrap(),
        };
        assert_eq!(Demand::decode(&demand_wire), Some(demand));
        assert_eq!(demand.encode().unwrap().as_ref(), demand_wire);
        assert_eq!(Ready::read_demand(&ready_wire), Some(demand));
        assert_eq!(Ready::decode(&ready_wire, parent), Some(ready));
        assert_eq!(ready.encode(parent).unwrap().as_ref(), ready_wire);
        assert_eq!(Ready::decode(&demand_wire, parent), None);
        assert_eq!(Demand::decode(&ready_wire), None);
        let refresh_wire = demand.encode_refresh();
        assert_eq!(Demand::decode_refresh(&refresh_wire), Some(demand));
        assert_eq!(Demand::decode_refresh(&demand_wire), None);
        assert_eq!(Demand::decode(&refresh_wire), None);
        assert_eq!(Ready::read_demand(&refresh_wire), None);
        assert_eq!(Demand::decode(&demand_wire[..37]), None);
        assert_eq!(Ready::decode(&ready_wire[..54], parent), None);

        let mut invalid = ready_wire.clone();
        invalid[51..].copy_from_slice(&demand.live.get().to_be_bytes());
        assert_eq!(Ready::decode(&invalid, parent), None);
        invalid = ready_wire.clone();
        invalid[40] = 0;
        assert_eq!(Ready::decode(&invalid, parent), None);
        invalid = ready_wire.clone();
        invalid[50] = 1;
        assert_eq!(Ready::decode(&invalid, parent), None);
        invalid = ready_wire.clone();
        invalid[30..38].copy_from_slice(&0x20000u64.to_be_bytes());
        assert_eq!(Ready::decode(&invalid, parent), None);
        invalid = ready_wire;
        invalid[22..30].fill(0);
        assert_eq!(Ready::read_demand(&invalid), None);
    }
}
