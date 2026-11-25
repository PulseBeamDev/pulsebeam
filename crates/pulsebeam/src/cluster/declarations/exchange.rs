use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use pulsebeam_link::{CONTROL_FRAME_BYTES, LinkTime, PeerControl};
use pulsebeam_runtime::time::Instant;
use serde::{Deserialize, Serialize};

use super::{Declaration, Error, Interest, Mode, Receive, Revision, Role};

const TAG: &[u8] = &[1, 0x10];
pub(crate) const MAX_INTERESTS: usize = 32;
const ANNOUNCE: Duration = Duration::from_secs(2);
const FRESH: Duration = Duration::from_secs(6);

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Frame {
    Interest {
        id: Revision,
        scope: Interest,
    },
    Withdraw {
        id: Revision,
    },
    Final {
        id: Revision,
        revision: Revision,
        present: bool,
    },
    Delta {
        id: Revision,
        base: Revision,
        revision: Revision,
        present: bool,
    },
}

impl Frame {
    fn send(&self, lane: &PeerControl, until: LinkTime) -> bool {
        let mut bytes = TAG.to_vec();
        serde_json::to_writer(&mut bytes, self).is_ok()
            && lane.try_send(bytes.into_boxed_slice(), until).is_ok()
    }
}

struct Requested {
    id: Revision,
    view: Receive,
    refresh: Option<Instant>,
    fresh: Option<Instant>,
    changed_at: Revision,
    replace_at: Instant,
}

impl Requested {
    fn observe(&mut self, now: Instant, revision: Revision, unchanged: bool) {
        if !unchanged {
            self.changed_at = revision;
        }
        self.fresh = now.checked_add(FRESH);
        self.replace_at = self.fresh.unwrap_or(now);
    }
}

pub(crate) struct Finalized {
    pub(crate) interest: Revision,
    pub(crate) changed_at: Revision,
    pub(crate) present: bool,
    pub(crate) fresh_until: Instant,
}

struct Published {
    scope: Interest,
    sent: Option<(Revision, bool)>,
    final_needed: bool,
    expires: Instant,
}

pub(crate) struct Exchange {
    local: Role,
    peer: Role,
    last_requested: u64,
    last_received: u64,
    requested: BTreeMap<Declaration, Requested>,
    published: BTreeMap<Revision, Published>,
}

impl Exchange {
    pub(crate) fn new(local: Role, peer: Role) -> Self {
        Self {
            local,
            peer,
            last_requested: 0,
            last_received: 0,
            requested: BTreeMap::new(),
            published: BTreeMap::new(),
        }
    }

    pub(crate) fn receive(&mut self, bytes: &[u8], now: Instant) -> bool {
        let Some(body) = bytes.strip_prefix(TAG) else {
            return false;
        };
        if bytes.len() <= CONTROL_FRAME_BYTES
            && let Ok(frame) = serde_json::from_slice(body)
        {
            self.apply(frame, now);
        }
        true
    }

    fn apply(&mut self, frame: Frame, now: Instant) {
        match frame {
            Frame::Interest { id, scope } => {
                if scope.room.is_none() || !self.local.adjacent(self.peer) {
                    return;
                }
                let Some(expires) = now.checked_add(FRESH) else {
                    return;
                };
                if let Some(existing) = self.published.get_mut(&id) {
                    if scope == existing.scope {
                        existing.final_needed = true;
                        existing.expires = expires;
                    }
                } else if id.get() > self.last_received && self.published.len() < MAX_INTERESTS {
                    self.last_received = id.get();
                    self.published.insert(
                        id,
                        Published {
                            scope,
                            sent: None,
                            final_needed: true,
                            expires,
                        },
                    );
                }
            }
            Frame::Withdraw { id } => {
                self.published.remove(&id);
            }
            Frame::Final {
                id,
                revision,
                present,
            } => {
                if let Some((&key, entry)) =
                    self.requested.iter_mut().find(|(_, entry)| entry.id == id)
                {
                    let unchanged = entry.fresh.is_some_and(|deadline| now < deadline)
                        && entry.view.ready().is_some_and(|(base, items)| {
                            base.get().checked_add(1) == Some(revision.get())
                                && items.contains(&key) == present
                        });
                    if entry.view.start(revision).is_ok() {
                        if present && entry.view.item(revision, key).is_err() {
                            entry.fresh = None;
                            return;
                        }
                        if entry.view.finish(revision, usize::from(present)).is_ok() {
                            entry.observe(now, revision, unchanged);
                        }
                    }
                }
            }
            Frame::Delta {
                id,
                base,
                revision,
                present,
            } => {
                if let Some((&key, entry)) =
                    self.requested.iter_mut().find(|(_, entry)| entry.id == id)
                {
                    if entry.fresh.is_none_or(|deadline| now >= deadline) {
                        entry.refresh = None;
                        return;
                    }
                    let unchanged = entry
                        .view
                        .ready()
                        .is_some_and(|(_, items)| items.contains(&key) == present);
                    match entry.view.delta(base, revision, key, present) {
                        Ok(()) => entry.observe(now, revision, unchanged),
                        Err(Error::Stale) => {}
                        Err(_) => {
                            entry.fresh = None;
                            entry.refresh = None;
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn step(
        &mut self,
        desired: &BTreeSet<Declaration>,
        has_local: impl Fn(Declaration) -> bool,
        lane: &PeerControl,
        expires: LinkTime,
        now: Instant,
    ) {
        self.published.retain(|_, entry| now < entry.expires);
        self.requested.retain(|key, entry| {
            (desired.contains(key) && now < entry.replace_at)
                || !(Frame::Withdraw { id: entry.id }).send(lane, expires)
        });
        for &key in desired.iter().take(MAX_INTERESTS) {
            let scope = Interest {
                project: key.project,
                room: Some(key.room),
                kind: key.kind,
                mode: Mode::CurrentFuture,
            };
            if let Some(entry) = self.requested.get_mut(&key) {
                if entry.refresh.is_some_and(|next| now < next) {
                    continue;
                }
                if !(Frame::Interest {
                    id: entry.id,
                    scope,
                })
                .send(lane, expires)
                {
                    break;
                }
                entry.refresh = now.checked_add(ANNOUNCE);
            } else {
                let Some(id) = self.last_requested.checked_add(1).and_then(Revision::new) else {
                    break;
                };
                if self.requested.len() >= MAX_INTERESTS {
                    break;
                }
                let Ok(view) = Receive::new(self.local, self.peer, scope) else {
                    break;
                };
                if !(Frame::Interest { id, scope }).send(lane, expires) {
                    break;
                }
                self.last_requested = id.get();
                self.requested.insert(
                    key,
                    Requested {
                        id,
                        view,
                        refresh: now.checked_add(ANNOUNCE),
                        fresh: None,
                        changed_at: Revision::MIN,
                        replace_at: now.checked_add(FRESH).unwrap_or(now),
                    },
                );
            }
        }
        for (&id, entry) in &mut self.published {
            let Some(room) = entry.scope.room else {
                continue;
            };
            let present = has_local(Declaration {
                project: entry.scope.project,
                room,
                kind: entry.scope.kind,
            });
            let previous = entry.sent;
            let Some(revision) = previous.map_or(Some(Revision::MIN), |(rev, _)| {
                rev.get().checked_add(1).and_then(Revision::new)
            }) else {
                continue;
            };
            let frame = match previous {
                _ if entry.final_needed => Frame::Final {
                    id,
                    revision,
                    present,
                },
                Some((base, before))
                    if present != before && entry.scope.mode == Mode::CurrentFuture =>
                {
                    Frame::Delta {
                        id,
                        base,
                        revision,
                        present,
                    }
                }
                _ => continue,
            };
            if !frame.send(lane, expires) {
                break;
            }
            entry.sent = Some((revision, present));
            entry.final_needed = false;
        }
    }

    pub(crate) fn published_queryable(
        &self,
        id: Revision,
        project: super::ProjectId,
        room: super::RoomId,
        now: Instant,
    ) -> bool {
        self.local.adjacent(self.peer)
            && self.published.get(&id).is_some_and(|entry| {
                now < entry.expires
                    && entry.scope.project == project
                    && entry.scope.room == Some(room)
                    && entry.scope.kind == super::Kind::Queryable
                    && entry.sent.is_some_and(|(_, present)| present)
            })
    }

    pub(crate) fn ready(&self, declaration: Declaration, now: Instant) -> Option<Finalized> {
        let entry = self.requested.get(&declaration)?;
        let fresh_until = entry.fresh.filter(|&deadline| now < deadline)?;
        let (_, items) = entry.view.ready()?;
        Some(Finalized {
            interest: entry.id,
            changed_at: entry.changed_at,
            present: items.contains(&declaration),
            fresh_until,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::declarations::Kind;
    use pulsebeam_core::identity::{ProjectId, RoomId};
    use pulsebeam_link::StreamControl;

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn scoped_interest_final_delta_and_withdraw_use_one_lane() {
        let limit = std::num::NonZeroUsize::new(pulsebeam_link::CONTROL_FRAME_BYTES).unwrap();
        let (lane_a, mut stream_a) = StreamControl::new_authenticated(limit);
        let (lane_b, mut stream_b) = StreamControl::new_authenticated(limit);
        let mut a = Exchange::new(Role::Edge, Role::Router);
        let mut b = Exchange::new(Role::Router, Role::Edge);
        let wanted = Declaration {
            project: ProjectId::new(),
            room: RoomId::new("watched").unwrap(),
            kind: Kind::Queryable,
        };
        let other = Declaration {
            room: RoomId::new("other").unwrap(),
            ..wanted
        };
        let desired = BTreeSet::from([wanted]);
        let now = Instant::now();
        let ttl = LinkTime::from_nanos(u64::MAX);
        a.step(&desired, |_| false, &lane_a, ttl, now);
        let (request, _) = stream_a.directions().0.recv().await.unwrap();
        assert!(b.receive(&request, now));
        assert!(a.ready(wanted, now).is_none());

        let idle = BTreeSet::new();
        b.step(&idle, |key| key == other, &lane_b, ttl, now);
        let (finalized, _) = stream_b.directions().0.recv().await.unwrap();
        b.step(&idle, |key| key == wanted, &lane_b, ttl, now);
        let (changed, _) = stream_b.directions().0.recv().await.unwrap();
        assert!(a.receive(&finalized, now));
        assert_eq!(
            a.ready(wanted, now)
                .map(|cut| (cut.changed_at, cut.present)),
            Some((Revision::MIN, false))
        );
        assert!(a.receive(&changed, now));
        assert_eq!(
            a.ready(wanted, now)
                .map(|cut| (cut.changed_at, cut.present)),
            Some((Revision::new(2).unwrap(), true))
        );
        assert!(a.ready(other, now).is_none());

        let now = now + ANNOUNCE;
        a.step(&desired, |_| false, &lane_a, ttl, now);
        let (refresh, _) = stream_a.directions().0.recv().await.unwrap();
        assert!(b.receive(&refresh, now));
        b.step(&idle, |key| key == wanted, &lane_b, ttl, now);
        let (unchanged, _) = stream_b.directions().0.recv().await.unwrap();
        assert!(a.receive(&unchanged, now));
        let cut = a.ready(wanted, now).unwrap();
        assert_eq!(cut.changed_at, Revision::new(2).unwrap());
        assert_eq!(cut.fresh_until, now + FRESH);

        a.step(&idle, |_| false, &lane_a, ttl, now);
        let (withdraw, _) = stream_a.directions().0.recv().await.unwrap();
        assert!(b.receive(&withdraw, now));
        assert!(b.receive(&request, now));
        assert!(b.published.is_empty());
        assert!(a.ready(wanted, now).is_none());
    }
}
