use super::*;

fn lookup() -> (Resource, wire::ResourceSpec) {
    (
        Resource::SourceLookup {
            room: "room".into(),
            participant_id: "bob".into(),
            kind: wire::TrackKind::Data,
            track_id: "updates".into(),
        },
        wire::ResourceSpec {
            settings: Some(wire::ResourceSettings {
                enabled: Some(true),
                receive: None,
            }),
            declaration: Some(wire::resource_spec::Declaration::SourceLookup(
                wire::SourceLookupSpec {
                    source: Some(wire::LogicalSourceAddress {
                        room: "room".into(),
                        participant_id: "bob".into(),
                        kind: wire::TrackKind::Data as i32,
                        track_id: "updates".into(),
                    }),
                },
            )),
        },
    )
}

fn confirmation(
    revision: u64,
    spec: &wire::ResourceSpec,
    source: Option<&str>,
) -> wire::ConfirmedState {
    wire::ConfirmedState {
        evaluated_revision: revision,
        rejection: None,
        value: Some(wire::confirmed_state::Value::SourceLookup(Box::new(
            wire::SourceLookupState {
                spec: Some(spec.clone()),
                participant: source.map(|_| wire::Participant {
                    participant_id: "bob".into(),

                    room_id: "room".into(),
                }),
                track: source.map(|id| wire::Track {
                    publication_id: id.into(),
                    participant_id: "bob".into(),
                    room_id: "room".into(),
                    track_id: "updates".into(),
                    kind: Some(wire::track::Kind::Data(wire::DataTrack {
                        capture_clock: None,
                        delivery: wire::DataDelivery::Ordered as i32,
                        record_dd: false,
                    })),
                }),
            },
        ))),
    }
}

#[test]
fn lookup_key_rejects_address_changes_before_staging() {
    for field in 0..4 {
        let mut client = NativeState::new(64 * 1024, participant()).unwrap();
        let (resource, mut spec) = lookup();
        let Some(wire::resource_spec::Declaration::SourceLookup(lookup)) =
            spec.declaration.as_mut()
        else {
            panic!("lookup declaration");
        };
        let source = lookup.source.as_mut().unwrap();
        match field {
            0 => source.room = "other-room".into(),
            1 => source.participant_id = "other-participant".into(),
            2 => source.kind = wire::TrackKind::Audio as i32,
            _ => source.track_id = "other-label".into(),
        }
        assert!(matches!(
            client.stage(resource, Some(spec)),
            Err(SignalingError::Invalid("resource correlation"))
        ));
        assert_eq!(client.retained_bytes, 0);
        assert!(client.next_frame().unwrap().is_none());
    }
}

#[test]
fn lookup_replacement_updates_private_indexes_without_discovery_or_execution() {
    let mut client = NativeState::new(64 * 1024, participant()).unwrap();
    client
        .apply(&frame(alloc::vec![session(1, 2_000_000_000, None)], true))
        .unwrap();
    let baseline = client.retained_bytes;
    let (resource, spec) = lookup();
    assert!(client.lookup(&resource).is_none());
    client.stage(resource.clone(), Some(spec.clone())).unwrap();
    assert_eq!(next(&mut client)[0].0, 1);
    for (index, source) in [None, Some("first"), None, Some("replacement")]
        .into_iter()
        .enumerate()
    {
        let state = confirmation(1, &spec, source);
        let changes = client
            .apply(&frame(alloc::vec![record(1, state.clone())], false))
            .unwrap();
        assert!(matches!(
            changes.as_slice(),
            [Change::Owned { new_evaluation, retired_binding: None, .. }]
                if *new_evaluation == (index == 0)
        ));
        assert!(client.converged(&resource));
        assert!(client.eligible(&resource).is_none());
        assert!(client.participants().is_empty());
        assert!(client.tracks().is_empty());
        assert_eq!(client.source("first").is_some(), source == Some("first"));
        assert_eq!(
            client.source("replacement").is_some(),
            source == Some("replacement")
        );
        assert_eq!(
            client
                .source_participant(&ParticipantKey {
                    room_id: "room".into(),
                    participant_id: "bob".into()
                })
                .is_some(),
            source.is_some()
        );
        assert_eq!(
            client
                .lookup(&resource)
                .and_then(|lookup| lookup.track.as_ref())
                .map(|track| track.publication_id.as_str()),
            source
        );
        assert!(
            client
                .apply(&frame(alloc::vec![record(1, state)], false))
                .unwrap()
                .is_empty()
        );
    }
    let accepted = client.confirmed(&resource).unwrap().clone();
    let retained = client.retained_bytes;
    let mut aliased = confirmation(1, &spec, Some("replacement"));
    let Some(wire::confirmed_state::Value::SourceLookup(lookup)) = aliased.value.as_mut() else {
        panic!("lookup confirmation");
    };
    lookup.participant.as_mut().unwrap().participant_id = "other-owner".into();
    lookup.track.as_mut().unwrap().participant_id = "other-owner".into();
    assert!(matches!(
        client.apply(
            &wire::ServerStateBatch {
                records: alloc::vec![record(1, aliased)],
                ..Default::default()
            }
            .encode_to_vec()
        ),
        Err(SignalingError::Malformed)
    ));
    assert_eq!(client.confirmed(&resource), Some(&accepted));
    assert_eq!(client.retained_bytes, retained);
    let other_room = Resource::SourceLookup {
        room: "other-room".into(),
        participant_id: "bob".into(),
        kind: wire::TrackKind::Data,
        track_id: "updates".into(),
    };
    let other_participant = Resource::SourceLookup {
        room: "room".into(),
        participant_id: "other-participant".into(),
        kind: wire::TrackKind::Data,
        track_id: "updates".into(),
    };
    assert!(client.lookup(&other_room).is_none());
    assert!(client.lookup(&other_participant).is_none());
    let mut disabled = spec.clone();
    disabled.settings.as_mut().unwrap().enabled = Some(false);
    client
        .stage(resource.clone(), Some(disabled.clone()))
        .unwrap();
    assert!(client.lookup(&resource).is_none());
    assert_eq!(next(&mut client)[0].1.revision, 2);
    client
        .apply(&frame(
            alloc::vec![record(1, confirmation(2, &disabled, None))],
            false,
        ))
        .unwrap();
    assert!(client.converged(&resource));
    assert!(client.lookup(&resource).is_none());
    assert!(client.source("replacement").is_none());
    client.stage(resource.clone(), Some(spec.clone())).unwrap();
    assert_eq!(next(&mut client)[0].1.revision, 3);
    client
        .apply(&frame(
            alloc::vec![record(1, confirmation(3, &spec, Some("latest")))],
            false,
        ))
        .unwrap();
    client.stage(resource, None).unwrap();
    assert!(client.cursor.is_none());
    assert_eq!(next(&mut client)[0].1.revision, 4);
    client
        .apply(&frame(alloc::vec![record(1, absent(4))], false))
        .unwrap();
    assert!(client.source("latest").is_none());
    assert!(client.source_references.is_empty());
    assert!(client.participant_references.is_empty());
    assert_eq!(client.retained_bytes, baseline);
    assert!(matches!(
        client.apply(&frame(
            alloc::vec![record(1, confirmation(3, &spec, Some("latest")))],
            false
        )),
        Err(SignalingError::Invalid("unknown owned reference"))
    ));
    assert_eq!(client.retained_bytes, baseline);
}
