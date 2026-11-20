use super::*;

fn begin_join(agent: &mut Agent, room: &str) -> (Generation, OperationId) {
    agent.command(AgentCommand::Join(room.into())).unwrap();
    let Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) = next_effect(agent) else {
        panic!("expected offer");
    };
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: offer(),
            resources: resources(channel(1)),
        }))
        .unwrap();
    let (operation, request) = native_request(agent, generation);
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["room"], room);
    assert_eq!(agent.snapshot().desired_room.as_deref(), Some(room));
    (generation, operation)
}

#[test]
fn join_validates_before_mutation_and_retries_a_settled_denial_once() {
    let mut agent = Agent::new(config()).unwrap();
    for room in [
        String::new(),
        "a\n".into(),
        "é".into(),
        "room/main".into(),
        "A".repeat(37),
    ] {
        let before = agent.snapshot().clone();
        assert!(agent.command(AgentCommand::Join(room)).is_err());
        assert_eq!(agent.snapshot(), &before);
        assert!(agent.next_effect().is_none());
    }
    let room = "A_".repeat(18);
    let (generation, operation) = begin_join(&mut agent, &room);
    assert_eq!(agent.snapshot().join_revision, 1);
    assert_eq!(agent.snapshot().evaluated_join_revision, 0);
    assert_eq!(agent.snapshot().accepted_join_revision, None);
    agent.command(AgentCommand::Join(room.clone())).unwrap();
    assert_eq!(agent.snapshot().join_revision, 1);
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: HttpResponse {
                status: 403,
                headers: vec![],
                body: vec![],
            },
        }))
        .unwrap();
    assert_eq!(
        agent.snapshot().terminal_failure.as_ref().unwrap().class,
        FailureClass::Authorization
    );
    assert_eq!(agent.snapshot().evaluated_join_revision, 1);
    assert_eq!(agent.snapshot().accepted_join_revision, None);
    let outcome = agent.snapshot().credential_outcome.as_ref().unwrap();
    assert_eq!(outcome.revision, 0);
    assert_eq!(outcome.status, CredentialStatus::Rejected);
    assert_eq!(
        outcome.error.as_ref().unwrap().class,
        FailureClass::Authorization
    );
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close { generation })
    );
    assert!(agent.next_effect().is_none());
    let (next, _) = begin_join(&mut agent, &room);
    assert_ne!(next, generation);
    assert_eq!(agent.snapshot().join_revision, 2);
    assert_eq!(agent.snapshot().evaluated_join_revision, 1);
    assert_eq!(agent.snapshot().accepted_join_revision, None);
    agent.command(AgentCommand::Join(room.clone())).unwrap();
    assert_eq!(agent.snapshot().join_revision, 2);
    assert!(agent.next_effect().is_none());
}

#[test]
fn credential_admission_denial_retries_only_new_intent_and_preserves_its_scope() {
    let post = |agent: &mut Agent| {
        let Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) = next_effect(agent) else {
            panic!("offer");
        };
        agent
            .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
                generation,
                offer: offer(),
                resources: resources(channel(1)),
            }))
            .unwrap();
        let (operation, request) = native_request(agent, generation);
        assert_eq!(request.method, HttpMethod::Post);
        (generation, operation, request)
    };
    for (replacement, early, status) in [
        (false, false, 401),
        (false, true, 403),
        (true, false, 403),
        (true, true, 401),
    ] {
        let (mut agent, incumbent) = if replacement {
            let (agent, generation, _) = native_connection::connected(Default::default());
            (agent, Some(generation))
        } else {
            (Agent::new(config()).unwrap(), None)
        };
        if replacement {
            agent.command(AgentCommand::Reconnect).unwrap();
        } else {
            agent.command(AgentCommand::Join("room".into())).unwrap();
        }
        let (denied_generation, denied, _) = post(&mut agent);
        let join_revision = agent.snapshot().join_revision;
        if early {
            agent
                .command(AgentCommand::RenewAuthorization("newest".into()))
                .unwrap();
        }
        drain_notifications(&mut agent);
        agent
            .handle(HostEvent::Http(HttpEvent::Response {
                operation: denied,
                response: HttpResponse {
                    status,
                    headers: vec![],
                    body: vec![],
                },
            }))
            .unwrap();
        let outcome = agent.snapshot().credential_outcome.as_ref().unwrap();
        assert_eq!(outcome.revision, 0);
        assert_eq!(outcome.status, CredentialStatus::Rejected);
        assert_eq!(
            agent.snapshot().accepted_credential_revision,
            incumbent.map(|_| 0)
        );
        if !early {
            assert_eq!(
                agent.snapshot().connection,
                ConnectionState::TerminalFailure
            );
            let before = agent.snapshot().clone();
            assert_eq!(
                agent.command(AgentCommand::RenewAuthorization(String::new())),
                Err(AgentError::InvalidAuthorizationToken)
            );
            assert_eq!(agent.snapshot(), &before);
            agent
                .command(AgentCommand::RenewAuthorization("private-token".into()))
                .unwrap();
        } else {
            assert!(
                !drain_notifications(&mut agent)
                    .iter()
                    .any(|notification| matches!(
                        notification,
                        Notification::Failure(_)
                            | Notification::ConnectionStateChanged {
                                to: ConnectionState::TerminalFailure,
                                ..
                            }
                    ))
            );
        }
        assert_eq!(agent.snapshot().credential_revision, 1);
        assert_eq!(agent.snapshot().join_revision, join_revision);
        assert!(agent.snapshot().terminal_failure.is_none());
        assert_eq!(
            agent.snapshot().connection,
            if replacement {
                ConnectionState::Reconnecting
            } else {
                ConnectionState::CreatingOffer
            }
        );
        assert_eq!(
            next_effect(&mut agent),
            Effect::Rtc(RtcEffect::Close {
                generation: denied_generation
            })
        );
        let (generation, operation, request) = post(&mut agent);
        assert_ne!(generation, denied_generation);
        let expected = if early {
            "Bearer newest"
        } else {
            "Bearer private-token"
        };
        assert!(
            request
                .headers
                .iter()
                .any(|header| header.name == "Authorization" && header.value == expected)
        );
        activate_connection(&mut agent, generation, operation, channel(1));
        assert_eq!(agent.snapshot().accepted_credential_revision, Some(1));
        assert_eq!(
            agent
                .snapshot()
                .credential_outcome
                .as_ref()
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            agent.snapshot().credential_outcome.as_ref().unwrap().status,
            CredentialStatus::Accepted
        );
        if let Some(incumbent) = incumbent {
            assert_eq!(
                next_effect(&mut agent),
                Effect::Rtc(RtcEffect::Close {
                    generation: incumbent
                })
            );
        }
        assert!(agent.next_effect_after_turn().is_none());
        let before = agent.snapshot().clone();
        agent
            .handle(HostEvent::Http(HttpEvent::Response {
                operation: denied,
                response: HttpResponse {
                    status,
                    headers: vec![],
                    body: vec![],
                },
            }))
            .unwrap();
        agent
            .handle(HostEvent::Rtc(RtcEvent::Closed {
                generation: denied_generation,
            }))
            .unwrap();
        assert_eq!(agent.snapshot(), &before);
        assert!(agent.next_effect_after_turn().is_none());
    }
    let mut agent = Agent::new(config()).unwrap();
    let (generation, operation) = begin_join(&mut agent, "room");
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: HttpResponse {
                status: 403,
                headers: vec![],
                body: vec![],
            },
        }))
        .unwrap();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close { generation })
    );
    agent.command(AgentCommand::Disconnect).unwrap();
    agent
        .command(AgentCommand::RenewAuthorization("future".into()))
        .unwrap();
    assert_eq!(agent.snapshot().connection, ConnectionState::Disconnected);
    assert!(
        agent.next_effect_after_turn().is_none(),
        "credential intent cannot undo disconnect"
    );
}

#[test]
fn latest_join_fences_old_admission_and_coalesces_while_closing() {
    let mut agent = Agent::new(config()).unwrap();
    let (old, operation) = begin_join(&mut agent, "room");
    agent
        .command(AgentCommand::Join("intermediate".into()))
        .unwrap();
    assert_eq!(
        next_effect(&mut agent),
        Effect::Rtc(RtcEffect::Close { generation: old })
    );
    assert_eq!(
        next_effect(&mut agent),
        Effect::Http(HttpEffect::Cancel { operation })
    );
    agent.command(AgentCommand::Disconnect).unwrap();
    agent
        .command(AgentCommand::RenewAuthorization("replacement-token".into()))
        .unwrap();
    let before = agent.snapshot().version;
    agent.command(AgentCommand::Join("final".into())).unwrap();
    assert!(agent.snapshot().version > before);
    assert_eq!(agent.snapshot().join_revision, 3);
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: create_response("p1", "unused", PARTICIPANT_URI),
        }))
        .unwrap();
    let Effect::Http(HttpEffect::Request { request, .. }) = next_effect(&mut agent) else {
        panic!("expected orphan cleanup");
    };
    assert_eq!(request.method, HttpMethod::Delete);
    assert_eq!(request.uri, PARTICIPANT_URI);
    assert!(
        request
            .headers
            .iter()
            .any(|header| header.name == "Authorization" && header.value == "Bearer private-token")
    );
    assert!(agent.snapshot().participant_id.is_none());
    assert!(agent.snapshot().room_id.is_none());
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::Rtc(RtcEvent::Closed { generation: old }))
        .unwrap();
    let (current, _) = begin_join(&mut agent, "final");
    assert_ne!(current, old);
    assert_eq!(agent.snapshot().join_revision, 3);
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation: old,
            offer: offer(),
            resources: resources(channel(1)),
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
}

#[test]
fn matching_join_activates_and_cannot_reclaim_after_supersession() {
    let mut agent = Agent::new(config()).unwrap();
    assert_eq!(agent.snapshot().credential_revision, 0);
    assert_eq!(agent.snapshot().accepted_credential_revision, None);
    agent
        .command(AgentCommand::RenewAuthorization("before-admission".into()))
        .unwrap();
    assert_eq!(agent.snapshot().credential_revision, 1);
    let before = agent.snapshot().clone();
    agent
        .command(AgentCommand::RenewAuthorization("before-admission".into()))
        .unwrap();
    assert_eq!(agent.snapshot(), &before);
    assert_eq!(
        agent.command(AgentCommand::RenewAuthorization(String::new())),
        Err(AgentError::InvalidAuthorizationToken)
    );
    assert_eq!(agent.snapshot(), &before);
    let (generation, operation) = begin_join(&mut agent, "room");
    agent
        .command(AgentCommand::RenewAuthorization("while-admitting".into()))
        .unwrap();
    assert_eq!(agent.snapshot().credential_revision, 2);
    assert_eq!(agent.snapshot().accepted_credential_revision, None);
    assert!(agent.next_effect().is_none());
    activate_connection(&mut agent, generation, operation, channel(1));
    assert_eq!(agent.snapshot().credential_revision, 2);
    assert_eq!(agent.snapshot().accepted_credential_revision, Some(1));
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert_eq!(agent.snapshot().room_id.as_deref(), Some("room"));
    assert_eq!(agent.snapshot().desired_room.as_deref(), Some("room"));
    assert_eq!(
        agent.snapshot().evaluated_join_revision,
        agent.snapshot().join_revision
    );
    assert_eq!(
        agent.snapshot().accepted_join_revision,
        Some(agent.snapshot().join_revision)
    );
    agent
        .host_failed(Failure {
            class: FailureClass::ParticipantSuperseded,
            message: "replaced".into(),
        })
        .unwrap();
    while agent.next_effect().is_some() {}
    let before = agent.snapshot().clone();
    for room in ["other", ""] {
        assert_eq!(
            agent.command(AgentCommand::Join(room.into())),
            Err(AgentError::Replaced),
        );
    }
    for token in ["cannot-revive", ""] {
        assert_eq!(
            agent.command(AgentCommand::RenewAuthorization(token.into())),
            Err(AgentError::Replaced),
        );
    }
    assert_eq!(agent.snapshot(), &before);
    assert!(agent.next_effect().is_none());
}

#[test]
fn unresolved_admissions_are_bounded_without_forgetting_late_cleanup() {
    let mut agent = Agent::new(config()).unwrap();
    let mut oldest = None;
    for index in 0..crate::agent::MAX_ORPHANED_CREATES {
        let room = format!("room-{index}");
        let (generation, operation) = begin_join(&mut agent, &room);
        oldest.get_or_insert(operation);
        agent
            .command(AgentCommand::Join(format!("room-{}", index + 1)))
            .unwrap();
        assert_eq!(
            next_effect(&mut agent),
            Effect::Rtc(RtcEffect::Close { generation })
        );
        assert_eq!(
            next_effect(&mut agent),
            Effect::Http(HttpEffect::Cancel { operation })
        );
        agent
            .handle(HostEvent::Rtc(RtcEvent::Closed { generation }))
            .unwrap();
    }
    assert_eq!(
        agent.snapshot().terminal_failure.as_ref().unwrap().class,
        FailureClass::InvalidConfiguration
    );
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: oldest.unwrap(),
            response: create_response("p1", "unused", PARTICIPANT_URI),
        }))
        .unwrap();
    let Effect::Http(HttpEffect::Request { request, .. }) = next_effect(&mut agent) else {
        panic!("expected retained cleanup");
    };
    assert_eq!(request.method, HttpMethod::Delete);
    assert_eq!(request.uri, PARTICIPANT_URI);
    assert!(agent.next_effect().is_none());
    begin_join(
        &mut agent,
        &format!("room-{}", crate::agent::MAX_ORPHANED_CREATES),
    );
}

#[test]
fn unsuccessful_admission_evaluates_only_its_captured_credential_and_cannot_enable_denial_retry() {
    const SECRET: &str = "bootstrap-request-secret";
    for (status, class) in [
        (503, FailureClass::Transient),
        (201, FailureClass::Protocol),
        (400, FailureClass::InvalidConfiguration),
        (409, FailureClass::ParticipantSuperseded),
    ] {
        let mut agent = Agent::new(config()).unwrap();
        agent
            .command(AgentCommand::RenewAuthorization(SECRET.into()))
            .unwrap();
        let (generation, operation) = begin_join(&mut agent, "room");
        agent
            .command(AgentCommand::RenewAuthorization(
                "bootstrap-latest-secret".into(),
            ))
            .unwrap();
        agent
            .handle(HostEvent::Http(HttpEvent::Response {
                operation,
                response: HttpResponse {
                    status,
                    headers: vec![HttpHeader {
                        name: "Location".into(),
                        value: PARTICIPANT_URI.into(),
                    }],
                    body: SECRET.as_bytes().to_vec(),
                },
            }))
            .unwrap();
        let outcome = agent.snapshot().credential_outcome.as_ref().unwrap();
        assert_eq!(outcome.revision, 1);
        assert_eq!(outcome.status, CredentialStatus::Uncertain);
        assert_eq!(outcome.error.as_ref().unwrap().class, class);
        assert_eq!(agent.snapshot().credential_revision, 2);
        assert_eq!(agent.snapshot().accepted_credential_revision, None);
        let notifications = drain_notifications(&mut agent);
        let observed = format!("{:?} {:?}", agent.snapshot(), notifications);
        assert!(!observed.contains(SECRET));
        assert!(!observed.contains("bootstrap-latest-secret"));
        if class != FailureClass::Transient {
            assert_eq!(
                next_effect(&mut agent),
                Effect::Rtc(RtcEffect::Close { generation })
            );
        }
        let timer = if class == FailureClass::Transient {
            let Effect::Timer(TimerEffect::Schedule { timer, after }) = next_effect(&mut agent)
            else {
                panic!("bounded existing retry");
            };
            assert!((Duration::from_millis(250)..=Duration::from_millis(500)).contains(&after));
            assert_eq!(
                agent.snapshot().connection,
                ConnectionState::RetryWaiting { attempt: 1, after }
            );
            Some(timer)
        } else {
            assert_eq!(
                agent.snapshot().connection,
                ConnectionState::TerminalFailure
            );
            assert_eq!(
                agent.snapshot().terminal_failure.as_ref().unwrap().class,
                class
            );
            None
        };
        let outcome = agent.snapshot().credential_outcome.clone();
        let state = agent.snapshot().connection.clone();
        let renewal = agent.command(AgentCommand::RenewAuthorization(
            "bootstrap-next-secret".into(),
        ));
        assert_eq!(
            renewal,
            if class == FailureClass::ParticipantSuperseded {
                Err(AgentError::Replaced)
            } else {
                Ok(())
            }
        );
        assert_eq!(agent.snapshot().credential_outcome, outcome);
        assert_eq!(agent.snapshot().connection, state);
        assert_eq!(
            agent.snapshot().credential_revision,
            if class == FailureClass::ParticipantSuperseded {
                2
            } else {
                3
            }
        );
        assert!(
            agent.next_effect_after_turn().is_none(),
            "uncertainty cannot establish the definitive-denial retry point"
        );
        if let Some(timer) = timer {
            agent.command(AgentCommand::Disconnect).unwrap();
            assert_eq!(
                next_effect(&mut agent),
                Effect::Timer(TimerEffect::Cancel { timer })
            );
            assert_eq!(
                next_effect(&mut agent),
                Effect::Rtc(RtcEffect::Close { generation })
            );
            let before = agent.snapshot().clone();
            agent
                .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
                .unwrap();
            assert_eq!(agent.snapshot(), &before);
            assert!(agent.next_effect_after_turn().is_none());
        }
    }
}

#[test]
fn mismatched_admission_room_is_cleaned_up_without_applying_answer() {
    let mut agent = Agent::new(config()).unwrap();
    let (_, operation) = begin_join(&mut agent, "selected");
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: create_response("p1", "unused", PARTICIPANT_URI),
        }))
        .unwrap();
    let effects: Vec<_> = core::iter::from_fn(|| agent.next_effect()).collect();
    assert!(effects.iter().any(|effect| matches!(effect, Effect::Http(HttpEffect::Request { request, .. }) if request.method == HttpMethod::Delete && request.uri == PARTICIPANT_URI)));
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Rtc(RtcEffect::ApplyAnswer { .. })))
    );
    assert_eq!(
        agent.snapshot().terminal_failure.as_ref().unwrap().class,
        FailureClass::Protocol
    );
    assert_eq!(agent.snapshot().desired_room.as_deref(), Some("selected"));
    assert!(agent.snapshot().room_id.is_none());
    let outcome = agent.snapshot().credential_outcome.as_ref().unwrap();
    assert_eq!(outcome.revision, 0);
    assert_eq!(outcome.status, CredentialStatus::Uncertain);
    assert_eq!(
        outcome.error.as_ref().unwrap().class,
        FailureClass::Protocol
    );
    assert_eq!(agent.snapshot().accepted_credential_revision, None);
}
