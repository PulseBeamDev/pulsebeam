use std::ops::Bound::{Excluded, Unbounded};

use pulsebeam_link::{LinkTime, PeerControl};

use super::*;

const TAG: &[u8] = &[1, 0x12];
const FRESH: Duration = Duration::from_secs(8);
const ANNOUNCE: Duration = Duration::from_secs(2);
const RECHECK: Duration = Duration::from_millis(150);
const REPORTS_PER_FRAME: usize = 16;
const MAX_FRAME: usize = 32 * 1024;

#[cfg(test)]
mod tests;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Report {
    origin: Member,
    role: Role,
    revision: NonZeroU64,
    fresh_ms: u16,
    links: Vec<(Member, NonZeroU16)>,
}

struct Record {
    advertisement: Advertisement,
    revision: NonZeroU64,
    fresh_until: Instant,
    learned_from: Option<Member>,
}

impl Record {
    fn stamp(&self) -> (NonZeroU64, NonZeroU64) {
        (self.advertisement.origin.generation, self.revision)
    }

    fn report(&self, now: Instant) -> Option<Report> {
        let fresh_ms =
            u16::try_from(self.fresh_until.saturating_duration_since(now).as_millis()).ok()?;
        (fresh_ms > 0).then(|| Report {
            origin: self.advertisement.origin,
            role: self.advertisement.role,
            revision: self.revision,
            fresh_ms,
            links: self
                .advertisement
                .neighbors
                .iter()
                .map(|(&peer, &cost)| (peer, cost))
                .collect(),
        })
    }
}

/// One authenticated regional stream, with a bounded flood cursor.
pub(crate) struct Outbound {
    peer: Member,
    role: Role,
    last: Option<NodeId>,
    sent: BTreeMap<NodeId, (NonZeroU64, NonZeroU64)>,
}

impl Outbound {
    pub(crate) fn new(peer: Member, role: Role) -> Self {
        Self {
            peer,
            role,
            last: None,
            sent: BTreeMap::new(),
        }
    }
}

/// Remote reports are soft state. Retain each admitted origin's high-water
/// mark after expiry so a duplicate or a delayed flood cannot reopen a link.
/// Relayed reports are routing hints attested by the pinned regional router;
/// they convey no publication authority or permission to install a branch.
pub(crate) struct Exchange {
    local: Member,
    role: Role,
    records: BTreeMap<NodeId, Record>,
    local_revision: u64,
    announce_at: Instant,
    check_at: Instant,
    routes: Routes,
}

impl Exchange {
    pub(crate) fn new(local: Member, role: Role, now: Instant) -> Self {
        Self {
            local,
            role,
            records: BTreeMap::new(),
            local_revision: 0,
            announce_at: now,
            check_at: now,
            routes: Routes::default(),
        }
    }

    pub(crate) fn identity(&self) -> (Member, Role) {
        (self.local, self.role)
    }

    pub(crate) fn reconcile(&mut self, links: BTreeMap<Member, NonZeroU16>, now: Instant) {
        if links.len() > MAX_PROVISIONED_PEERS {
            return;
        }
        let changed = self
            .records
            .get(&self.local.node)
            .is_none_or(|own| own.advertisement.neighbors != links);
        if changed || now >= self.announce_at {
            let Some(revision) = self.local_revision.checked_add(1).and_then(NonZeroU64::new)
            else {
                return;
            };
            let Some(fresh_until) = now.checked_add(FRESH) else {
                return;
            };
            self.local_revision = revision.get();
            self.announce_at = now.checked_add(ANNOUNCE).unwrap_or(now);
            self.records.insert(
                self.local.node,
                Record {
                    advertisement: Advertisement {
                        origin: self.local,
                        role: self.role,
                        neighbors: links,
                    },
                    revision,
                    fresh_until,
                    learned_from: None,
                },
            );
        }
        if changed || now >= self.check_at {
            let live = self
                .records
                .values()
                .filter(|rec| now < rec.fresh_until)
                .map(|rec| &rec.advertisement);
            if let Some(graph) = Graph::from_reports(live) {
                self.routes.reconcile(self.local, &graph, now);
            }
            self.check_at = now.checked_add(RECHECK).unwrap_or(now);
        }
    }

    pub(crate) fn path(&self, destination: Member) -> Option<&[Member]> {
        self.routes.path(destination)
    }

    pub(crate) fn receive(&mut self, from: &Outbound, frame: &[u8], now: Instant) -> bool {
        let Some(body) = frame.strip_prefix(TAG) else {
            return false;
        };
        if frame.len() <= MAX_FRAME
            && self.role.adjacent(from.role)
            && let Ok(reports) = serde_json::from_slice::<Vec<Report>>(body)
            && reports.len() <= REPORTS_PER_FRAME
        {
            for report in reports {
                self.accept(from, report, now);
            }
        }
        true
    }

    fn accept(&mut self, from: &Outbound, report: Report, now: Instant) {
        if report.origin.node == self.local.node
            || report.links.len() > MAX_PROVISIONED_PEERS
            || if report.origin.node == from.peer.node {
                report.origin != from.peer || report.role != from.role
            } else {
                from.role != Role::Router
            }
        {
            return;
        }
        let received_for = Duration::from_millis(u64::from(report.fresh_ms));
        if received_for.is_zero() || received_for > FRESH {
            return;
        }
        let Some(fresh_until) = now.checked_add(received_for) else {
            return;
        };
        let stamp = (report.origin.generation, report.revision);
        let existing = self.records.get(&report.origin.node);
        if existing.is_some_and(|current| {
            current.stamp() >= stamp
                || (current.stamp().0 == stamp.0 && current.advertisement.role != report.role)
        }) || (existing.is_none()
            && self.records.len()
                >= MAX_MEMBERS
                    .saturating_sub(usize::from(!self.records.contains_key(&self.local.node))))
        {
            return;
        }
        let count = report.links.len();
        let neighbors: BTreeMap<_, _> = report.links.into_iter().collect();
        if neighbors.len() != count || neighbors.keys().any(|peer| peer.node == report.origin.node)
        {
            return;
        }
        self.records.insert(
            report.origin.node,
            Record {
                advertisement: Advertisement {
                    origin: report.origin,
                    role: report.role,
                    neighbors,
                },
                revision: report.revision,
                fresh_until,
                learned_from: Some(from.peer),
            },
        );
        self.check_at = now;
    }

    pub(crate) fn flush(
        &self,
        outbound: &mut Outbound,
        lane: &PeerControl,
        until: LinkTime,
        now: Instant,
    ) {
        let ordered = self
            .records
            .range((outbound.last.map_or(Unbounded, Excluded), Unbounded))
            .chain(
                outbound
                    .last
                    .into_iter()
                    .flat_map(|last| self.records.range(..=last)),
            );
        let reports: Vec<_> = ordered
            .filter_map(|(node, record)| {
                if *node == outbound.peer.node
                    || record.learned_from == Some(outbound.peer)
                    || (*node != self.local.node && self.role != Role::Router)
                    || outbound
                        .sent
                        .get(node)
                        .is_some_and(|stamp| *stamp >= record.stamp())
                {
                    return None;
                }
                record.report(now)
            })
            .take(REPORTS_PER_FRAME)
            .collect();
        if reports.is_empty() {
            return;
        }
        let mut frame = TAG.to_vec();
        if serde_json::to_writer(&mut frame, &reports).is_ok()
            && frame.len() <= MAX_FRAME
            && lane.try_send(frame.into_boxed_slice(), until).is_ok()
        {
            for report in reports {
                outbound.last = Some(report.origin.node);
                outbound.sent.insert(
                    report.origin.node,
                    (report.origin.generation, report.revision),
                );
            }
        }
    }
}
