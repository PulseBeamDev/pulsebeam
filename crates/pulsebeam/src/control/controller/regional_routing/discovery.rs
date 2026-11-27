use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::Duration,
};

use pulsebeam_core::{auth::ApiSigningKey, identity::NodeId};
use pulsebeam_runtime::time::Instant;

use crate::{
    clock::reference::Snapshot,
    cluster::{
        RegionTree,
        admission::DiscoveryPort,
        boot_announcement::{Announcement, FRAME_BYTES, Scope, VerifiedBoot},
        mesh::Member,
    },
};

use super::{Connected, Link, Links};

const TAG: &[u8; 6] = b"PBDF\x01\x00";
const BATCH: usize = 8;
const RECEIVE_BUDGET: usize = 64;
const FLUSH: Duration = Duration::from_millis(30);
const REPORT: Duration = Duration::from_secs(1);
const REFRESH_MS: u64 = 3_000;
const MAX_UNCERTAINTY_NS: u64 = 150_000_000;

#[cfg(all(test, feature = "sim"))]
mod tests;

type Stamp = (std::num::NonZeroU64, u64);

pub(super) fn recognizes(frame: &[u8]) -> bool {
    frame.starts_with(TAG)
}

struct Record {
    stamp: Stamp,
    expires_ms: u64,
    frame: [u8; FRAME_BYTES],
    pending: Option<VerifiedBoot>,
}

pub(super) enum Change {
    Admit(Member),
    Withdraw(Member),
}

#[derive(Clone, Copy)]
struct Current {
    member: Member,
    expires_ms: u64,
    deadline: Instant,
}

pub(super) struct Discovery {
    local: Member,
    scope: Scope,
    key: ApiSigningKey,
    direct: BTreeSet<NodeId>,
    clock: Snapshot,
    clocks: pulsebeam_queue::Receiver<Snapshot>,
    port: DiscoveryPort,
    records: BTreeMap<NodeId, Record>,
    current: BTreeMap<NodeId, Current>,
    sent: BTreeMap<Link, BTreeMap<NodeId, Stamp>>,
    issue_at: u64,
    flush_at: Option<Instant>,
    report_at: Option<Instant>,
    time_floor_ms: u64,
}

impl Discovery {
    pub(super) fn new(
        local: Member,
        scope: Scope,
        key: ApiSigningKey,
        direct: BTreeSet<NodeId>,
        clock: Snapshot,
        clocks: pulsebeam_queue::Receiver<Snapshot>,
        port: DiscoveryPort,
    ) -> Self {
        Self {
            local,
            scope,
            key,
            direct,
            clock,
            clocks,
            port,
            records: BTreeMap::new(),
            current: BTreeMap::new(),
            sent: BTreeMap::new(),
            issue_at: 0,
            flush_at: None,
            report_at: None,
            time_floor_ms: 0,
        }
    }

    fn time(&mut self, now: Instant) -> Option<u64> {
        while let Ok(clock) = self.clocks.try_recv() {
            self.clock = clock;
        }
        let estimate = self.clock.at(now)?;
        if estimate.uncertainty_ns? > MAX_UNCERTAINTY_NS {
            return None;
        }
        let time_ms = u64::try_from(estimate.global.to_tai_1970_nanos().ok()?).ok()? / 1_000_000;
        self.time_floor_ms = self.time_floor_ms.max(time_ms);
        Some(self.time_floor_ms)
    }

    pub(super) fn drive(
        &mut self,
        tree: &RegionTree,
        links: &Connected<'_>,
        inbox: &mut VecDeque<(Link, Box<[u8]>)>,
        now: Instant,
    ) -> Vec<Change> {
        let mut changes = Vec::new();
        let time = self.time(now);
        self.current.retain(|_, entry| {
            let alive = time.is_some_and(|wall| wall < entry.expires_ms) && now < entry.deadline;
            if !alive {
                changes.push(Change::Withdraw(entry.member));
            }
            alive
        });
        let mut budget = RECEIVE_BUDGET;
        inbox.retain(|(from, bytes)| {
            let Some(batch) = bytes.strip_prefix(TAG) else {
                return true;
            };
            if let Some(time) = time
                && links.link(from.member) == Some(*from)
                && batch.len().is_multiple_of(FRAME_BYTES)
                && batch.len() <= BATCH * FRAME_BYTES
            {
                for frame in batch.chunks_exact(FRAME_BYTES).take(budget) {
                    budget = budget.saturating_sub(1);
                    self.observe(tree, frame, time);
                }
            }
            false
        });
        if let Some(time) = time {
            if time >= self.issue_at {
                let frame = Announcement::encode(self.scope, self.local, time, &self.key);
                self.remember(
                    time,
                    self.local,
                    frame,
                    time.saturating_add(crate::cluster::boot_announcement::VALID_FOR_MS),
                    None,
                );
                self.issue_at = time.saturating_add(REFRESH_MS);
            }
            self.submit(time);
            self.flush(links, now, time);
        }
        for _ in 0..RECEIVE_BUDGET {
            let Ok(admitted) = self.port.admitted.try_recv() else {
                break;
            };
            let proof = admitted.into_boot();
            let member = proof.origin;
            let previous = self.current.get(&member.node).copied();
            if previous.is_some_and(|old| member.generation < old.member.generation) {
                continue;
            }
            if let Some(old) = previous
                && old.member != member
            {
                changes.push(Change::Withdraw(old.member));
                self.current.remove(&member.node);
            }
            let Some(deadline) = time
                .and_then(|wall| proof.expires_ms.checked_sub(wall))
                .filter(|remaining| *remaining != 0)
                .and_then(|remaining| now.checked_add(Duration::from_millis(remaining)))
            else {
                continue;
            };
            if self
                .current
                .get(&member.node)
                .is_some_and(|old| old.expires_ms >= proof.expires_ms)
            {
                continue;
            }
            if previous.is_none_or(|old| old.member != member) {
                changes.push(Change::Admit(member));
            }
            self.current.insert(
                member.node,
                Current {
                    member,
                    expires_ms: proof.expires_ms,
                    deadline,
                },
            );
        }
        self.report(tree, links, time.is_some(), now);
        changes
    }

    fn report(&mut self, tree: &RegionTree, links: &Connected<'_>, qualified: bool, now: Instant) {
        if self.report_at.is_some_and(|next| now < next) {
            return;
        }
        self.report_at = now.checked_add(REPORT);
        let live = links.discovery_links().len();
        let expected = self.direct.len();
        let total = tree.discovery_slots().count();
        let routed = total.saturating_sub(expected.saturating_add(1));
        let discovered = self.current.len();
        for (kind, available, unavailable) in [
            ("direct", live, expected.saturating_sub(live)),
            ("routed", discovered, routed.saturating_sub(discovered)),
        ] {
            metrics::gauge!("regional_control_boot_slots", "kind" => kind, "state" => "available")
                .set(available as f64);
            metrics::gauge!("regional_control_boot_slots", "kind" => kind, "state" => "unavailable").set(unavailable as f64);
        }
        metrics::gauge!("regional_boot_discovery_clock_qualified").set(if qualified {
            1.0
        } else {
            0.0
        });
    }

    fn observe(&mut self, tree: &RegionTree, frame: &[u8], time: u64) {
        let Some(announcement) = Announcement::decode(frame) else {
            return;
        };
        let origin = announcement.origin();
        if origin.node == self.local.node
            || self
                .records
                .get(&origin.node)
                .is_some_and(|record| record.stamp >= announcement.stamp())
        {
            return;
        }
        let Some(proof) = tree
            .discovery_key(origin)
            .and_then(|key| announcement.verify(self.scope, key, time))
        else {
            return;
        };
        let Ok(frame) = <[u8; FRAME_BYTES]>::try_from(frame) else {
            return;
        };
        let (issued, expires) = (proof.issued_ms, proof.expires_ms);
        let pending = (!self.direct.contains(&origin.node)).then_some(proof);
        self.remember(issued, origin, frame, expires, pending);
    }

    fn remember(
        &mut self,
        issued: u64,
        origin: Member,
        frame: [u8; FRAME_BYTES],
        expires_ms: u64,
        pending: Option<VerifiedBoot>,
    ) {
        self.records.insert(
            origin.node,
            Record {
                stamp: (origin.generation, issued),
                expires_ms,
                frame,
                pending,
            },
        );
    }

    fn submit(&mut self, time: u64) {
        for record in self
            .records
            .values_mut()
            .filter(|record| time < record.expires_ms && record.pending.is_some())
            .take(BATCH)
        {
            let Ok(slot) = self.port.candidates.try_reserve() else {
                break;
            };
            if let Some(pending) = record.pending.take() {
                slot.send(pending);
            }
        }
    }

    fn flush(&mut self, links: &Connected<'_>, now: Instant, time: u64) {
        if self.flush_at.is_some_and(|at| now < at) {
            return;
        }
        self.flush_at = now.checked_add(FLUSH);
        let connected = links.discovery_links();
        self.sent.retain(|link, _| connected.contains(link));
        for link in connected {
            let sent = self.sent.entry(link).or_default();
            let mut batch = Vec::new();
            let mut updates = Vec::new();
            for (&node, record) in self
                .records
                .iter()
                .filter(|(node, record)| {
                    time < record.expires_ms
                        && sent.get(node).is_none_or(|&stamp| stamp < record.stamp)
                })
                .take(BATCH)
            {
                if batch.is_empty() {
                    batch.extend_from_slice(TAG);
                }
                batch.extend_from_slice(&record.frame);
                updates.push((node, record.stamp));
            }
            if !batch.is_empty()
                && let Some(expires) = now.checked_add(Duration::from_secs(1))
                && links.send(link, batch.into_boxed_slice(), expires)
            {
                sent.extend(updates);
            }
        }
    }
}
