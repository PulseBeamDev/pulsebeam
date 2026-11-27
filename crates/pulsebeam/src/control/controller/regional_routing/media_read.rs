//! Owner reads are independent of ownership images. Each read scans a bounded
//! number of locally indexed names and samples at most one local publisher.

use std::{
    collections::BTreeMap,
    num::NonZeroU64,
    time::{Duration, SystemTime},
};

use prost::Message;
use pulsebeam_core::auth::{ApiSigningKey, grants::SubscribeScope};
use pulsebeam_proto::state::Track;
use pulsebeam_runtime::{sync::oneshot, time::Instant};
use serde::{Deserialize, Serialize};

use crate::{
    control::{controller::ControllerActor, topology::NamedTrack},
    entity::{ProjectId, ProjectRoomKey},
    shard_update::{PublisherAnchor, PublisherPosition},
};

use super::{
    Member, RegionTree,
    forwarding::{Links, Router, Ticket},
    media_permit,
};
use crate::control::controller::peer_ownership::relay::wan_source::WanSourceWindow;

const GET: &[u8] = &[1, 0x16, 0];
const REPLY: &[u8] = &[1, 0x16, 1];
const MAX_PAYLOAD: usize = crate::cluster::REGIONAL_FRAME_BYTES - 64;
const MAX_PENDING: usize = 16;
const MAX_CURSORS: usize = 128;
const CURSOR_TTL: Duration = Duration::from_secs(3);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Get {
    pub(super) id: NonZeroU64,
    pub(super) project: ProjectId,
    pub(super) scope: SubscribeScope,
    pub(super) after: Option<NonZeroU64>,
    pub(super) expires_ms: u64,
    pub(super) demand_expires_ms: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reply {
    pub(super) id: NonZeroU64,
    pub(super) page: Option<Page>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Page {
    pub(super) next: Option<NonZeroU64>,
    pub(super) row: Option<Row>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Row {
    descriptor: Box<[u8]>,
    pub(super) permit: Box<[u8]>,
}

impl Row {
    pub(super) fn new(descriptor: &Track, permit: Box<[u8]>) -> Self {
        Self {
            descriptor: descriptor.encode_to_vec().into_boxed_slice(),
            permit,
        }
    }

    fn empty_proof() -> Self {
        Self {
            descriptor: Box::default(),
            permit: Box::default(),
        }
    }

    pub(super) fn proves_empty(&self) -> bool {
        self.descriptor.is_empty() && self.permit.is_empty()
    }

    pub(super) fn descriptor(&self) -> Option<Track> {
        Track::decode(self.descriptor.as_ref()).ok()
    }
}

fn encode(tag: &[u8], value: &impl Serialize) -> Option<Box<[u8]>> {
    let mut bytes = tag.to_vec();
    serde_json::to_writer(&mut bytes, value).ok()?;
    (bytes.len() <= MAX_PAYLOAD).then_some(bytes.into_boxed_slice())
}

fn decode<'a, T: Deserialize<'a>>(tag: &[u8], bytes: &'a [u8]) -> Option<T> {
    if bytes.len() > MAX_PAYLOAD {
        return None;
    }
    serde_json::from_slice(bytes.strip_prefix(tag)?).ok()
}

impl Get {
    pub(super) fn encode(&self) -> Option<Box<[u8]>> {
        encode(GET, self)
    }

    pub(super) fn decode(bytes: &[u8]) -> Option<Self> {
        decode(GET, bytes)
    }
}

impl Reply {
    fn encode(&self) -> Option<Box<[u8]>> {
        encode(REPLY, self)
    }

    pub(super) fn decode(bytes: &[u8]) -> Option<Self> {
        decode(REPLY, bytes)
    }
}

struct Cursor {
    expires: Instant,
    project: ProjectId,
    scope: SubscribeScope,
    name: NamedTrack,
}

struct Pending {
    ticket: Ticket,
    id: NonZeroU64,
    next: Option<NonZeroU64>,
    room: ProjectRoomKey,
    publisher: PublisherAnchor,
    scope: SubscribeScope,
    sample: oneshot::Receiver<Option<PublisherPosition>>,
}

#[derive(Default)]
pub(super) struct OwnerReads {
    cursors: BTreeMap<NonZeroU64, Cursor>,
    cursor_serial: u64,
    queued: Vec<(Ticket, Get)>,
    pending: Vec<Pending>,
    replies: Vec<(Ticket, Reply)>,
    anchors: BTreeMap<
        crate::control::controller::peer_ownership::relay::Key,
        (WanSourceWindow, Instant),
    >,
}

impl OwnerReads {
    pub(super) fn enqueue(&mut self, ticket: Ticket, get: Get) {
        if self.pending.len().saturating_add(self.queued.len()) < MAX_PENDING {
            self.queued.push((ticket, get));
        }
    }

    pub(super) fn stage(&mut self, actor: &mut ControllerActor, now: Instant, wall: SystemTime) {
        if self.queued.is_empty() {
            return;
        }
        self.cursors.retain(|_, cursor| now < cursor.expires);
        let mut samples = Vec::new();
        for (ticket, get) in self.queued.drain(..) {
            let Some(wall_ms) = media_permit::unix_millis(wall) else {
                continue;
            };
            let Some(expires) = get
                .expires_ms
                .checked_sub(wall_ms)
                .map(Duration::from_millis)
                .map(|ttl| ttl.min(ticket.expires().saturating_duration_since(now)))
                .and_then(|ttl| now.checked_add(ttl))
                .filter(|&expires| now < expires)
            else {
                continue;
            };
            let after = if let Some(id) = get.after {
                let cursor = self
                    .cursors
                    .get(&id)
                    .filter(|cursor| cursor.project == get.project && cursor.scope == get.scope);
                let Some(cursor) = cursor else {
                    self.replies.push((
                        ticket,
                        Reply {
                            id: get.id,
                            page: None,
                        },
                    ));
                    continue;
                };
                Some(cursor.name)
            } else {
                None
            };
            let (source, next, empty_confirmed) = actor.lifecycle.scan_owned_subscribe_scope(
                get.project,
                &get.scope,
                after,
                &actor.core.registry,
            );
            let next = if let Some(name) = next {
                // Continuations cannot disclose the name of a filtered-out source.
                let Some(expires) = now.checked_add(CURSOR_TTL) else {
                    continue;
                };
                let Some(id) = self.cursor_serial.checked_add(1).and_then(NonZeroU64::new) else {
                    continue;
                };
                if self.cursors.len() >= MAX_CURSORS {
                    self.cursors.pop_first();
                }
                self.cursor_serial = id.get();
                self.cursors.insert(
                    id,
                    Cursor {
                        expires,
                        project: get.project,
                        scope: get.scope.clone(),
                        name,
                    },
                );
                Some(id)
            } else {
                None
            };
            if let Some((room, shard, publisher)) = source {
                samples.push((
                    shard,
                    publisher,
                    get.demand_expires_ms
                        .and_then(|deadline| deadline.checked_sub(wall_ms))
                        .and_then(|ttl| now.checked_add(Duration::from_millis(ttl)))
                        .map(|deadline| deadline.min(expires)),
                    (ticket, get.id, next, room, publisher, get.scope),
                ));
            } else {
                self.replies.push((
                    ticket,
                    Reply {
                        id: get.id,
                        // The empty proof has no installable descriptor or permit, so older
                        // clients skip it like an unresolved row but still follow the cursor.
                        page: Some(Page {
                            next,
                            row: empty_confirmed.then(Row::empty_proof),
                        }),
                    },
                ));
            }
        }
        if !samples.is_empty() {
            self.pending
                .extend(actor.stage_anchor_samples(samples).into_iter().map(
                    |((ticket, id, next, room, publisher, scope), sample)| Pending {
                        ticket,
                        id,
                        next,
                        room,
                        publisher,
                        scope,
                        sample,
                    },
                ));
        }
    }

    pub(super) fn flush(
        &mut self,
        actor: &ControllerActor,
        router: &Router<'_, impl Links>,
        key: &ApiSigningKey,
        now: Instant,
        wall_now: SystemTime,
    ) {
        self.anchors.retain(|_, (_, until)| now < *until);
        let Some(anchor_expiry) = now.checked_add(CURSOR_TTL) else {
            return;
        };
        self.pending.retain_mut(|pending| {
            if !pending.ticket.live(now) {
                return false;
            }
            let row = match pending.sample.try_recv() {
                Err(oneshot::error::TryRecvError::Empty) => return true,
                Ok(Some(sample)) => {
                    let mut source = WanSourceWindow::from_sample(
                        pending.room,
                        pending.publisher,
                        sample.sequence,
                    )
                    .with_projection(sample.timing)
                    .with_ladder(sample.ladder);
                    let scope = source.scope();
                    if let Some((previous, until)) = self.anchors.get_mut(&scope) {
                        if previous.publisher == source.publisher
                            && previous.window_start == source.window_start
                        {
                            source = previous.with_ladder(source.ladder);
                        } else {
                            *previous = source;
                        }
                        *until = anchor_expiry;
                    } else if self.anchors.len()
                        < crate::control::controller::peer_ownership::relay::LIMIT
                    {
                        self.anchors.insert(scope, (source, anchor_expiry));
                    }
                    issue_row(
                        actor,
                        router.tree,
                        router.local,
                        key,
                        source,
                        &pending.scope,
                        wall_now,
                    )
                }
                _ => None,
            };
            self.replies.push((
                pending.ticket,
                Reply {
                    id: pending.id,
                    page: Some(Page {
                        next: pending.next,
                        row,
                    }),
                },
            ));
            false
        });
        for (ticket, mut reply) in self.replies.drain(..) {
            let encoded = reply.encode().or_else(|| {
                reply.page.as_mut()?.row = None;
                reply.encode()
            });
            if let Some(body) = encoded {
                router.reply(ticket, &body, now);
            }
        }
    }
}

fn issue_row(
    actor: &ControllerActor,
    tree: &RegionTree,
    origin: Member,
    key: &ApiSigningKey,
    source: WanSourceWindow,
    scope: &SubscribeScope,
    wall_now: SystemTime,
) -> Option<Row> {
    let current = actor
        .lifecycle
        .source_exact(source.room, source.publisher.publication)?;
    if !scope.allows(
        source.room.room.as_str(),
        current.meta().origin.as_str(),
        crate::control::permissions::grant_kind(current.kind()),
        current.meta().track_id?.as_str(),
    ) {
        return None;
    }
    let (descriptor, permit) = media_permit::issue(actor, tree, origin, key, source, wall_now)?;
    Some(Row::new(&descriptor, permit))
}

#[cfg(test)]
mod tests {
    use pulsebeam_core::auth::grants::{Action, Grant, GrantSet};

    use super::*;

    #[test]
    fn owner_read_messages_carry_only_attenuated_grants_and_opaque_continuations() {
        let project = ProjectId::new();
        let make_grant =
            |allow| Grant::new(vec!["fleet/*".into()], allow, None, None, None).unwrap();
        let grants =
            GrantSet::new(vec![make_grant(vec![Action::Discover, Action::Subscribe])]).unwrap();
        let scope = grants
            .subscribe_scope(Some("fleet/a"), None, None, None)
            .unwrap();
        let get = Get {
            id: NonZeroU64::MIN,
            project,
            scope: scope.clone(),
            after: NonZeroU64::new(3),
            expires_ms: 1234,
            demand_expires_ms: Some(1200),
        };
        let wire = get.encode().unwrap();
        let decoded = Get::decode(&wire).unwrap();
        assert_eq!(decoded.id, get.id);
        assert_eq!(decoded.project, project);
        assert_eq!(decoded.scope, scope);
        assert_eq!(decoded.after, get.after);
        assert_eq!(decoded.expires_ms, get.expires_ms);
        assert!(Reply::decode(&wire).is_none());
        let unauthorized =
            serde_json::json!({"id":1, "project":project, "scope":grants, "after":null});
        assert!(Get::decode(&encode(GET, &unauthorized).unwrap()).is_none());

        let descriptor = Track {
            track_id: "feed".into(),
            ..Track::default()
        };
        let reply = Reply {
            id: get.id,
            page: Some(Page {
                next: NonZeroU64::new(4),
                row: Some(Row {
                    descriptor: descriptor.encode_to_vec().into_boxed_slice(),
                    permit: Box::from([7, 9]),
                }),
            }),
        };
        let wire = reply.encode().unwrap();
        assert!(Get::decode(&wire).is_none());
        let decoded = Reply::decode(&wire).unwrap();
        assert_eq!(decoded.id, get.id);
        let page = decoded.page.unwrap();
        assert_eq!(page.next, NonZeroU64::new(4));
        let row = page.row.unwrap();
        assert_eq!(row.descriptor().unwrap(), descriptor);
        assert_eq!(row.permit.as_ref(), &[7, 9]);
    }
}
