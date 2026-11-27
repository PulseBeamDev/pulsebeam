use std::{cell::RefCell, net::SocketAddr, num::NonZeroU32};

use pulsebeam_core::fabric::{Profile, wan::Window};
use pulsebeam_link::{NetworkId, wan::Ecn};

use crate::route::fabric::local::OwnedPacket;

thread_local! {
    static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
    static PROTECTED: RefCell<Option<ProtectedProbe>> = const { RefCell::new(None) };
}

type Edge = (SocketAddr, SocketAddr);

struct ProtectedProbe {
    edges: [Edge; 2],
    target: Option<ProtectedReport>,
    networks: [Option<NetworkId>; 2],
    selected: Option<(NetworkId, NetworkId)>,
}

#[derive(Clone, Copy, Debug)]
pub struct ProtectedReport {
    pub position: u64,
    pub timestamp: u32,
    pub dropped_live: [usize; 2],
    pub independent_offers: usize,
    pub repaired_timestamp: Option<u32>,
}

pub fn arm_protected_h264(primary: Edge, standby: Edge) {
    PROTECTED.with_borrow_mut(|probe| {
        *probe = Some(ProtectedProbe {
            edges: [primary, standby],
            target: None,
            networks: [None; 2],
            selected: None,
        });
    });
}

pub fn protected_report() -> Option<ProtectedReport> {
    PROTECTED.with_borrow(|probe| probe.as_ref()?.target)
}

pub(crate) fn protected_edge(peer: SocketAddr, local: SocketAddr) -> bool {
    PROTECTED.with_borrow(|probe| {
        probe
            .as_ref()
            .is_some_and(|probe| probe.edges.contains(&(peer, local)))
    })
}

pub(crate) fn select_protected_base(position: u64, timestamp: u32) {
    PROTECTED.with_borrow_mut(|probe| {
        if let Some(probe) = probe {
            probe.target.get_or_insert(ProtectedReport {
                position,
                timestamp,
                dropped_live: [0; 2],
                independent_offers: 0,
                repaired_timestamp: None,
            });
        }
    });
}

pub(crate) fn protected_loss(edge: Edge, network: NetworkId, position: u64, repair: bool) -> bool {
    PROTECTED.with_borrow_mut(|probe| {
        let Some(probe) = probe else {
            return false;
        };
        let Some(index) = probe.edges.iter().position(|&candidate| candidate == edge) else {
            return false;
        };
        let Some(target) = probe
            .target
            .as_mut()
            .filter(|target| target.position == position)
        else {
            return false;
        };
        probe.networks[index] = Some(network);
        if repair {
            // In addition to losing the exact live position, the primary is
            // unavailable for this repair. The independent cache is intact.
            return index == 0;
        }
        target.dropped_live[index] += 1;
        true
    })
}

pub(crate) fn selected_independent_repair(original: NetworkId, other: NetworkId, position: u64) {
    PROTECTED.with_borrow_mut(|probe| {
        if let Some(probe) = probe
            && probe
                .target
                .is_some_and(|target| target.position == position)
            && probe.networks == [Some(original), Some(other)]
        {
            probe.selected = Some((original, other));
        }
    });
}

pub(crate) fn admitted_protected_repair(
    peer: NetworkId,
    position: u64,
    timestamp: u32,
    enqueued: usize,
) {
    PROTECTED.with_borrow_mut(|probe| {
        if let Some(probe) = probe
            && probe
                .selected
                .is_some_and(|(old, other)| old != other && other == peer)
            && let Some(target) = probe
                .target
                .as_mut()
                .filter(|target| target.position == position)
        {
            target.independent_offers += enqueued;
            target.repaired_timestamp = Some(timestamp);
        }
    });
}

struct Probe {
    from: SocketAddr,
    to: SocketAddr,
    after: Option<u64>,
    profile: Profile,
    lose_first_repair: bool,
    target: Option<Target>,
}

struct Target {
    association: NetworkId,
    held: Option<(Box<[u8]>, Option<Ecn>)>,
    repaired_packet: Option<OwnedPacket>,
    report: Report,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub position: u64,
    pub live: NonZeroU32,
    pub live_wire_timestamp: u32,
    pub repaired_timestamp: Option<u32>,
    pub dropped_repair: Option<NonZeroU32>,
    pub opened_repair: Option<NonZeroU32>,
    pub late_original_matches: Option<bool>,
    pub repair_offers: usize,
    pub late_original_offers: Option<usize>,
    pub repair_admitted: bool,
    pub dd_cursor_before_first_offer: Option<u64>,
    pub rtc_enqueues: usize,
}

pub fn arm(from: SocketAddr, to: SocketAddr, after: Option<u64>) {
    arm_probe(from, to, after, Profile::Opus, true);
}

pub fn arm_vp9(from: SocketAddr, to: SocketAddr) {
    arm_probe(from, to, None, Profile::Vp9, false);
}

fn arm_probe(
    from: SocketAddr,
    to: SocketAddr,
    after: Option<u64>,
    profile: Profile,
    lose_first_repair: bool,
) {
    PROBE.with_borrow_mut(|probe| {
        *probe = Some(Probe {
            from,
            to,
            after,
            profile,
            lose_first_repair,
            target: None,
        });
    });
}

pub fn report() -> Option<Report> {
    PROBE.with_borrow(|probe| Some(probe.as_ref()?.target.as_ref()?.report.clone()))
}

pub(crate) fn active(from: SocketAddr, to: SocketAddr) -> bool {
    PROBE.with_borrow(|probe| {
        probe
            .as_ref()
            .is_some_and(|probe| probe.from == from && probe.to == to)
    })
}

pub(crate) fn filter(
    association: NetworkId,
    bytes: Box<[u8]>,
    ecn: Option<Ecn>,
    window: Window,
    position: u64,
    wire_timestamp: u32,
    repair: bool,
) -> Option<Box<[u8]>> {
    let label = window.ssrc();
    PROBE.with_borrow_mut(|probe| {
        let Some(probe) = probe
            .as_mut()
            .filter(|probe| probe.profile == window.profile())
        else {
            return Some(bytes);
        };
        if !repair && probe.target.is_none() && probe.after.is_none_or(|after| position > after) {
            probe.target = Some(Target {
                association,
                held: Some((bytes, ecn)),
                repaired_packet: None,
                report: Report {
                    position,
                    live: label,
                    live_wire_timestamp: wire_timestamp,
                    repaired_timestamp: None,
                    dropped_repair: None,
                    opened_repair: None,
                    late_original_matches: None,
                    repair_offers: 0,
                    late_original_offers: None,
                    repair_admitted: false,
                    dd_cursor_before_first_offer: None,
                    rtc_enqueues: 0,
                },
            });
            return None;
        }
        if repair
            && probe.lose_first_repair
            && let Some(target) = &mut probe.target
            && target.association == association
            && target.report.position == position
            && *target.report.dropped_repair.get_or_insert(label) == label
        {
            return None;
        }
        Some(bytes)
    })
}

pub(crate) fn authenticated(
    association: NetworkId,
    window: Window,
    position: u64,
    packet: &OwnedPacket,
) {
    PROBE.with_borrow_mut(|probe| {
        let Some(probe) = probe
            .as_mut()
            .filter(|probe| probe.profile == window.profile())
        else {
            return;
        };
        let Some(target) = probe.target.as_mut() else {
            return;
        };
        if target.association != association || target.report.position != position {
            return;
        }
        if window.ssrc() == target.report.live {
            target.report.late_original_matches = Some(
                target
                    .repaired_packet
                    .as_ref()
                    .is_some_and(|repaired| repaired.same_content(packet)),
            );
        } else if !probe.lose_first_repair
            || target
                .report
                .dropped_repair
                .is_some_and(|dropped| dropped != window.ssrc())
        {
            target.report.opened_repair = Some(window.ssrc());
            target.report.repaired_timestamp = Some(packet.header().timestamp);
            target.repaired_packet = Some(packet.clone());
        }
    });
}

pub(crate) fn release_original(association: NetworkId) -> Option<(Box<[u8]>, Option<Ecn>)> {
    PROBE.with_borrow_mut(|probe| {
        let target = probe.as_mut()?.target.as_mut()?;
        target
            .report
            .opened_repair
            .filter(|_| target.association == association)?;
        target.held.take()
    })
}

pub(crate) fn dd_cursor_before_first_offer(position: u64, cursor: Option<u64>) {
    PROBE.with_borrow_mut(|probe| {
        if let Some(target) = probe
            .as_mut()
            .filter(|probe| probe.profile == Profile::Vp9)
            .and_then(|probe| probe.target.as_mut())
            && target.report.position == position
        {
            target.report.dd_cursor_before_first_offer = cursor;
        }
    });
}

pub(crate) fn vp9_rtc_enqueued(position: u64, timestamp: u32) {
    PROBE.with_borrow_mut(|probe| {
        if let Some(target) = probe
            .as_mut()
            .filter(|probe| probe.profile == Profile::Vp9)
            .and_then(|probe| probe.target.as_mut())
            && target.report.position == position
            && target.report.live_wire_timestamp == timestamp
        {
            target.report.rtc_enqueues += 1;
        }
    });
}

pub(crate) fn admitted(
    profile: Profile,
    association: NetworkId,
    position: u64,
    repair: bool,
    enqueued: usize,
) {
    PROBE.with_borrow_mut(|probe| {
        let Some(target) = probe
            .as_mut()
            .filter(|probe| probe.profile == profile)
            .and_then(|probe| probe.target.as_mut())
        else {
            return;
        };
        if target.association != association || target.report.position != position {
            return;
        }
        if repair {
            target.report.repair_admitted = true;
            target.report.repair_offers = target.report.repair_offers.saturating_add(enqueued);
        } else if target.report.late_original_matches.is_some() {
            target.report.late_original_offers = Some(enqueued);
        }
    });
}
