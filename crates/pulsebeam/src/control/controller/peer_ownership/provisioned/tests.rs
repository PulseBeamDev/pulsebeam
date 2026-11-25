use std::num::NonZeroU32;

use pulsebeam_core::fabric::{Profile, wan::Window};
use pulsebeam_link::NodeRouteKey;

use crate::{
    entity::{ConnectionId, TrackKind},
    id::ShardId,
    link::wan_host::SendFixture,
    route::fabric::{
        CompiledPublisherSend, LiveNextHop,
        local::{Emission, RepairFragment, RepairRequest},
        relay::{PeerRepairLookup, PublisherRepairLookup},
    },
    shard_update::PublisherAnchor,
};

use super::{fabric_data::*, *};

#[test]
fn lookup_accepts_reordered_fragments_but_keeps_replay_and_cancel_fences() {
    let now = Instant::now();
    let frame = |id, index| RepairFrame {
        scope: Scope {
            key: NodeRouteKey::from_bytes([0, 0, 0, 1]),
            source: NonZeroU64::MIN,
        },
        branch: NonZeroU64::MIN,
        request: RepairRequest {
            fragment: RepairFragment { unit_seq: 1, index },
            id: NonZeroU64::new(id).unwrap(),
        },
        action: RepairAction::Lookup,
    };
    let mut book = RepairLookups::default();
    let cancel = |frame| RepairFrame {
        action: RepairAction::Cancel,
        ..frame
    };
    assert_eq!(book.cancel(cancel(frame(3, 1)), now), None);
    let mut admitted = Vec::new();
    for lookup in [frame(4, 2), frame(2, 0)] {
        book.lookup(lookup, now, || {
            admitted.push(lookup.request.id.get());
            vec![oneshot::channel().1]
        });
    }
    assert_eq!(admitted, [4, 2]);
    for lookup in [frame(4, 2), frame(2, 2), frame(3, 1)] {
        book.lookup(lookup, now, || {
            panic!("duplicate or cancelled lookup staged")
        });
    }
    let high = 1 + u64::from(pulsebeam_proto::data_record::MAX_FRAGMENTS);
    assert_eq!(book.cancel(cancel(frame(high, 0)), now), None);
    book.lookup(frame(1, 0), now, || panic!("stale lookup staged"));
}

#[test]
fn ready_requires_an_announced_exact_single_use_location() {
    let now = Instant::now();
    let expires = now + MAX_REPAIR_LIFETIME;
    let ssrc = NonZeroU32::MIN;
    let window = Window::new(Profile::Data, 0, 0, ssrc, 0).unwrap();
    let (_fixture, peer, tx) = SendFixture::new(window, expires);
    let connection = ConnectionId::from_bytes([7; 16]);
    let scope = Scope {
        key: NodeRouteKey::from_bytes(ssrc.get().to_be_bytes()),
        source: NonZeroU64::MIN,
    };
    let lookup = RepairFrame {
        scope,
        branch: NonZeroU64::MIN,
        request: RepairRequest {
            fragment: RepairFragment {
                unit_seq: 3,
                index: 0,
            },
            id: NonZeroU64::MIN,
        },
        action: RepairAction::Lookup,
    };
    let cancel = RepairFrame {
        action: RepairAction::Cancel,
        ..lookup
    };
    let ready = |sequence| WanRepairReady {
        scope,
        branch: lookup.branch,
        request: lookup.request,
        sequence,
        window: window
            .repair_window(sequence, NonZeroU32::new(2).unwrap())
            .unwrap(),
    };
    let mut book = RepairLookups::default();
    let mut cancelled = RepairLookups::default();
    assert_eq!(cancelled.cancel(cancel, now), None);
    cancelled.lookup(lookup, now, || panic!("cancelled lookup cannot be staged"));
    let (result, receipt) = oneshot::channel();
    book.lookup(lookup, now, || vec![receipt]);
    assert!(
        result
            .send(Some(LocatedRepair {
                lookup: Box::new(PeerRepairLookup::Publisher(PublisherRepairLookup {
                    route: CompiledPublisherSend {
                        publisher: PublisherAnchor {
                            publication: connection.fixture_publication(TrackKind::Data, "repair"),
                            connection,
                            incarnation: scope.source,
                            emission: Emission::data(false),
                        },
                        shard: ShardId::new(0),
                        next: LiveNextHop {
                            peer,
                            key: scope.key
                        },
                        tx: tx.clone(),
                    },
                    request: lookup.request,
                    branch: lookup.branch,
                })),
                sequence: 1,
                expires,
            }))
            .is_ok()
    );
    let reject = |_| panic!("unannounced or mismatched location cannot hand off");
    book.ready_matching(ready(1), now, |_| true, reject);
    let mut replies = Vec::new();
    book.flush(now, |reply| {
        replies.push(reply);
        true
    });
    assert_eq!(
        replies,
        [RepairFrame {
            action: RepairAction::Located { sequence: 1 },
            ..lookup
        }]
    );
    let stale = RepairRequest {
        id: NonZeroU64::MAX,
        ..lookup.request
    };
    book.ready_matching(
        WanRepairReady {
            request: stale,
            ..ready(1)
        },
        now,
        |_| true,
        reject,
    );
    assert_eq!(
        book.cancel(
            RepairFrame {
                request: stale,
                ..cancel
            },
            now
        ),
        None
    );
    book.ready_matching(ready(2), now, |_| true, reject);
    book.ready_matching(ready(1), now, |_| false, reject);
    book.ready_matching(
        ready(1),
        now,
        |_| true,
        |located| {
            assert_eq!(located.sequence, 1);
            Some(tx.cutoff().id())
        },
    );
    book.ready_matching(ready(1), now, |_| true, reject);
    assert_eq!(book.cancel(cancel, now), Some(tx.cutoff().id()));
}
