use std::{num::NonZeroU64, time::Duration};

use pulsebeam_core::identity::NodeId;
use pulsebeam_proto::state::{DataDelivery, DataTrack, track::Kind};

use super::*;
use crate::{
    entity::{ConnectionId, ProjectId, ProjectRoomKey, RoomId},
    route::fabric::local::Emission,
    shard_update::PublisherAnchor,
};

#[test]
fn media_permit_pins_origin_incarnation_scope_window_descriptor_and_expiry() {
    let origin_key = ApiSigningKey::from_seed([1; 32]);
    let gateway_key = ApiSigningKey::from_seed([2; 32]);
    let origin = Member {
        node: NodeId::new(),
        generation: NonZeroU64::MIN,
    };
    let gateway = Member {
        node: NodeId::new(),
        generation: NonZeroU64::MIN,
    };
    let tree_for = |generation, key| {
        serde_json::from_value(serde_json::json!({
            "regions": [{"name":"region", "members":[
                {"node":origin.node, "generation":generation, "role":"router", "media_key":key},
                {"node":gateway.node, "generation":1, "role":"gateway"}
            ]}],
            "borders":[]
        }))
        .unwrap()
    };
    let tree = tree_for(1, Some(origin_key.public_key()));
    let endpoint = EndpointKey {
        participant: ParticipantId::new("alice").unwrap(),
        room: ProjectRoomKey {
            project: ProjectId::new(),
            room: RoomId::new("room").unwrap(),
        },
    };
    let track = TrackId::new("feed").unwrap();
    let connection = ConnectionId::new();
    let publisher = PublisherAnchor {
        connection,
        publication: connection.fixture_publication(TrackKind::Data, "feed"),
        emission: Emission::data(false),
        incarnation: NonZeroU64::new(7).unwrap(),
    };
    let source = WanSourceWindow {
        origin: Some(origin),
        ..WanSourceWindow::from_sample(endpoint.room, publisher, 42)
    };
    let descriptor = Track {
        publication_id: publisher.publication.as_str(),
        room_id: endpoint.room.room.as_str().into(),
        participant_id: endpoint.participant.as_str().into(),
        track_id: track.as_str().into(),
        kind: Some(Kind::Data(DataTrack {
            capture_clock: None,
            delivery: DataDelivery::Latest.into(),
            record_dd: false,
        })),
    };
    let assertion = SourceAssertion {
        endpoint,
        track,
        descriptor: &descriptor,
        source,
    };
    let now = UNIX_EPOCH + Duration::from_secs(10);
    let until = 11_000;
    let permit = sign(&tree, &origin_key, assertion, now, until).unwrap();
    let check = |tree: &RegionTree, member, endpoint, descriptor: &Track, payload: &[u8], time| {
        verify(tree, member, endpoint, track, descriptor, payload, time)
    };
    let live = check(&tree, origin, endpoint, &descriptor, &permit, now).unwrap();
    assert_eq!(live.source, source);
    assert_eq!(live.expires_ms, until);

    use crate::control::{
        controller::{
            native_service::MediaInterest,
            regional_routing::{media_client::Demand, media_read::Row},
        },
        permissions::{Component, SubscriptionSelector},
    };
    use pulsebeam_core::auth::grants::{Action, Grant, GrantSet};
    use pulsebeam_runtime::time::Instant;

    let interest = |project, participant| MediaInterest {
        owner: (ConnectionId::new(), 1),
        authorization: 1,
        project,
        selector: SubscriptionSelector {
            project: Component::Literal(project),
            room: Component::Literal(endpoint.room.room),
            participant: Component::Literal(ParticipantId::new(participant).unwrap()),
            kind: Component::Any,
            track: Component::Any,
        },
        scope: GrantSet::new(vec![
            Grant::new(
                vec!["room".into()],
                vec![Action::Subscribe],
                None,
                None,
                None,
            )
            .unwrap(),
        ])
        .unwrap()
        .subscribe_scope(None, Some(participant), None, None)
        .unwrap(),
    };
    let demand = |interests: &[MediaInterest]| {
        Demand::verify(
            &tree,
            origin,
            interests,
            Row::new(&descriptor, permit.clone()),
            Instant::now(),
            now,
        )
    };
    let verified = demand(&[interest(endpoint.room.project, "alice")]).unwrap();
    assert_eq!(verified.source, source);
    assert_eq!(verified.descriptor, descriptor);
    assert_eq!(verified.permit, permit);
    assert!(demand(&[interest(endpoint.room.project, "bob")]).is_none());
    assert!(demand(&[interest(ProjectId::new(), "alice")]).is_none());
    assert!(demand(&[]).is_none());
    check_branch(&tree, gateway, assertion, &permit, now);

    assert!(
        check(
            &tree,
            origin,
            endpoint,
            &descriptor,
            &permit,
            now + Duration::from_secs(1)
        )
        .is_none()
    );
    assert!(
        check(
            &tree,
            origin,
            endpoint,
            &descriptor,
            &permit,
            now - Duration::from_millis(1)
        )
        .is_none()
    );
    assert!(check(&tree, gateway, endpoint, &descriptor, &permit, now).is_none());
    assert!(
        check(
            &tree_for(1, None),
            origin,
            endpoint,
            &descriptor,
            &permit,
            now
        )
        .is_none()
    );
    assert!(
        check(
            &tree_for(2, Some(origin_key.public_key())),
            origin,
            endpoint,
            &descriptor,
            &permit,
            now
        )
        .is_none()
    );
    assert!(sign(&tree, &gateway_key, assertion, now, until).is_none());
    assert!(
        sign(
            &tree,
            &origin_key,
            SourceAssertion {
                source: WanSourceWindow {
                    offer_cut: Some(NonZeroU64::MIN),
                    ..source
                },
                ..assertion
            },
            now,
            until
        )
        .is_none()
    );
    let another = EndpointKey {
        room: ProjectRoomKey {
            project: ProjectId::new(),
            ..endpoint.room
        },
        ..endpoint
    };
    assert!(check(&tree, origin, another, &descriptor, &permit, now).is_none());
    let mismatch = Track {
        kind: Some(Kind::Data(DataTrack {
            capture_clock: None,
            delivery: DataDelivery::Lossy.into(),
            record_dd: false,
        })),
        ..descriptor.clone()
    };
    assert!(check(&tree, origin, endpoint, &mismatch, &permit, now).is_none());
    let (signed, signature) = permit.split_at(permit.len() - SIGNATURE_BYTES);
    let mut altered = DOMAIN.to_vec();
    let mut body: Statement = serde_json::from_slice(&signed[DOMAIN.len()..]).unwrap();
    body.source = WanSourceWindow {
        sequence_anchor: 43,
        ..source
    }
    .into();
    serde_json::to_writer(&mut altered, &body).unwrap();
    altered.extend_from_slice(signature);
    assert!(check(&tree, origin, endpoint, &descriptor, &altered, now).is_none());
}

fn check_branch(
    tree: &RegionTree,
    downstream: Member,
    assertion: SourceAssertion<'_>,
    permit: &[u8],
    wall: SystemTime,
) {
    use crate::control::controller::{
        peer_ownership::relay::fabric::WanLiveRouteAck,
        regional_routing::{
            forwarding::{Arrival, Forwarding, Link, Links, Router},
            frames::{Frame, HOPS, Request},
            media_branch::{self, Edge, Leases},
            media_read::Row,
        },
    };
    use pulsebeam_core::fabric::wan::Window;
    use pulsebeam_link::{LinkControl, LinkTime};
    use pulsebeam_runtime::time::Instant;
    use std::num::NonZeroU32;

    struct Direct(Link);
    impl Links for Direct {
        fn first_local_hop(&self, to: Member) -> Option<Member> {
            (to == self.0.member).then_some(to)
        }
        fn link(&self, to: Member) -> Option<Link> {
            (to == self.0.member).then_some(self.0)
        }
        fn send(&self, _: Link, _: Box<[u8]>, _: Instant) -> bool {
            false
        }
    }

    let source = assertion.source;
    let target = source.origin.unwrap();
    let local = target;
    let link = Link {
        member: downstream,
        stream: NonZeroU64::MIN,
    };
    let mut control = LinkControl::<()>::new(|| LinkTime::from_nanos(0)).unwrap();
    let edge = Edge {
        link,
        network: control.reserve_network_id().unwrap(),
    };
    let router = Router {
        local,
        tree,
        links: Direct(link),
    };
    let ack = WanLiveRouteAck {
        room: source.room,
        publication: source.publisher.publication,
        incarnation: source.publisher.incarnation,
        sequence_anchor: source.sequence_anchor,
        offer_cut: None,
        window: Window::new(
            source.publisher.emission.profile(),
            source.window_start,
            source.forms(),
            NonZeroU32::MIN,
            source.publisher.emission.wire_profile().1,
        )
        .unwrap(),
    };
    let now = Instant::now();
    let mut fwd = Forwarding::default();
    let mut leases = Leases::default();
    let mut deliver = |from, ack, visited| {
        let body =
            media_branch::encode(Row::new(assertion.descriptor, permit.into()), ack, visited)
                .unwrap();
        let frame = Frame::Branch(Request {
            target,
            border: None,
            budget: HOPS,
            attempt: 0,
            payload: &body,
        })
        .encode()
        .unwrap();
        if let Some(Arrival::Branch(request)) = fwd.receive(&router, from, &frame, now) {
            leases.receive(tree, local, edge, request, now, wall);
        }
        leases.iter().count()
    };
    assert_eq!(
        deliver(
            Link {
                stream: NonZeroU64::new(2).unwrap(),
                ..link
            },
            ack,
            vec![downstream]
        ),
        0
    );
    assert_eq!(deliver(link, ack, vec![local, downstream]), 0);
    assert_eq!(
        deliver(
            link,
            WanLiveRouteAck {
                sequence_anchor: ack.sequence_anchor + 1,
                ..ack
            },
            vec![downstream]
        ),
        0
    );
    assert_eq!(deliver(link, ack, vec![downstream]), 1);
    let lease = leases.iter().next().unwrap();
    assert_eq!(lease.downstream.network, edge.network);
    assert_eq!(lease.ack, ack);
    assert_eq!(lease.source, source);
    leases.expire(now + Duration::from_secs(1), wall + Duration::from_secs(1));
    assert_eq!(leases.iter().count(), 0);
}
