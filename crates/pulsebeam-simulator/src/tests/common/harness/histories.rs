use super::super::oracle::{ControlStateOracle, ExpectedParticipant};
use super::*;
use crate::tests::histories::{CAST, Claim, History, Op, Witness};
use anyhow::{Context, Result, ensure};

struct Bound {
    name: &'static str,
    expected: ExpectedParticipant,
    retired: Option<Instant>,
    crashed: bool,
    retirement_generation: Option<u64>,
}

struct Transition {
    bindings: Vec<Bound>,
    seen: BTreeMap<String, BTreeSet<String>>,
    tombstones: BTreeMap<String, BTreeSet<String>>,
    holds: BTreeMap<u8, Instant>,
    desired: Vec<String>,
    desired_tracks: Vec<String>,
    outstanding_changes: Vec<(u64, u64)>,
    admitting: BTreeSet<&'static str>,
    unresolved: BTreeSet<(&'static str, String)>,
    unresolved_publications: BTreeSet<(&'static str, String, String)>,
    stall: Option<Instant>,
    stall_count: u64,
    failed_count: u64,
    orphan_count: u64,
    failed: bool,
    recovery_at: Instant,
}

fn observation(handle: &ParticipantHandle) -> Option<ClientObservation> {
    handle.shared.observation.lock().unwrap().clone()
}

fn name(index: u8) -> &'static str {
    if index == 0 {
        "viewer"
    } else {
        CAST[usize::from(index - 1)]
    }
}

impl Transition {
    fn publication(&self, track: pulsebeam::entity::TrackId) -> Option<&Bound> {
        self.bindings.iter().find(|b| {
            b.expected.video
                && b.expected
                    .track_id(pulsebeam::entity::TrackKind::Video, "video")
                    == track
        })
    }

    fn bind(&mut self, handles: &PlanHandles) -> Result<()> {
        for (name, handle) in handles.iter().filter(|(_, h)| h.present) {
            let Some(observed) = observation(handle) else {
                continue;
            };
            let Some(id) = observed.snapshot.participant_id.as_deref() else {
                continue;
            };
            let Some(connection) = observed.admission else {
                continue;
            };
            if self
                .bindings
                .iter()
                .any(|b| b.expected.connection == Some(connection))
            {
                continue;
            }
            let expected = ExpectedParticipant {
                id: id.parse()?,
                room: handle.room_name,
                video: handle.publishes_video,
                audio: handle.publishes_audio,
                required: true,
                connection: Some(connection),
                topics: handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .intent
                    .topics
                    .clone(),
                video_targets: HashSet::new(),
            };
            self.bindings.push(Bound {
                name,
                expected,
                retired: None,
                crashed: false,
                retirement_generation: None,
            });
            self.admitting.remove(name);
        }
        self.unresolved.retain(|(room, id)| {
            !self
                .bindings
                .iter()
                .any(|b| b.expected.room == *room && b.expected.id.as_str() == *id)
        });
        self.unresolved_publications
            .retain(|(room, origin, track)| {
                !self.bindings.iter().any(|b| {
                    b.expected.room == *room
                        && b.expected.id.as_str() == *origin
                        && b.expected.video
                        && b.expected
                            .track_id(pulsebeam::entity::TrackKind::Video, "video")
                            .as_str()
                            == *track
                })
            });
        ensure!(
            self.unresolved_publications.is_empty() || !self.admitting.is_empty(),
            "unbound or invalid publication identity after admission: {:?}",
            self.unresolved_publications
        );
        ensure!(
            self.unresolved.is_empty() || !self.admitting.is_empty(),
            "unbound or foreign origins after admission: {:?}",
            self.unresolved
        );
        Ok(())
    }

    fn causal(
        &mut self,
        source: String,
        present: BTreeSet<String>,
        applied: Option<u64>,
    ) -> Result<()> {
        let retired = self
            .bindings
            .iter()
            .filter(|b| {
                b.retired.is_some()
                    && applied.is_none_or(|generation| {
                        b.retirement_generation
                            .is_some_and(|fence| generation >= fence)
                    })
            })
            .flat_map(|b| {
                [
                    format!(
                        "{}:{:?}",
                        b.expected.id.as_str(),
                        b.expected.connection.unwrap()
                    ),
                    b.expected
                        .track_id(pulsebeam::entity::TrackKind::Video, "video")
                        .as_str(),
                ]
            })
            .collect::<BTreeSet<_>>();
        let seen = self.seen.entry(source.clone()).or_default();
        let dead = self.tombstones.entry(source.clone()).or_default();
        ensure!(
            dead.is_disjoint(&present),
            "causally retired generation resurrected at {source}: {dead:?} vs {present:?}"
        );
        for id in seen.intersection(&retired) {
            if !present.contains(id) {
                dead.insert(id.clone());
            }
        }
        seen.extend(present);
        Ok(())
    }

    fn clients_safe(&mut self, handles: &PlanHandles) -> Result<()> {
        self.bind(handles)?;
        for (name, handle) in handles.iter().filter(|(_, h)| h.present) {
            let Some(observed) = observation(handle) else {
                continue;
            };
            let mut allowed = self
                .bindings
                .iter()
                .filter(|b| b.expected.room == handle.room_name)
                .map(|b| b.expected.id.as_str())
                .collect::<BTreeSet<_>>();
            // Peer Catalog can beat the admitting client's HTTP response. Retain unknown
            // origins until that value boundary binds them, rather than inventing an ID.
            for id in observed.snapshot.participants.keys().chain(
                observed
                    .snapshot
                    .publications
                    .values()
                    .map(|p| &p.participant_id),
            ) {
                if !allowed.contains(id)
                    && !self.admitting.is_empty()
                    && !self.bindings.iter().any(|b| b.expected.id.as_str() == *id)
                {
                    self.unresolved.insert((handle.room_name, id.clone()));
                    allowed.insert(id.clone());
                }
            }
            for publication in observed.snapshot.publications.values() {
                ensure!(
                    publication.kind == pulsebeam_agent_native::agent_core::MediaKind::Video
                        && publication.label == "video",
                    "unexpected history publication kind/label"
                );
                if !self.bindings.iter().any(|b| {
                    b.expected.room == handle.room_name
                        && b.expected.id.as_str() == publication.participant_id
                        && b.expected.video
                        && b.expected
                            .track_id(pulsebeam::entity::TrackKind::Video, "video")
                            .as_str()
                            == publication.id
                }) {
                    ensure!(
                        !self.admitting.is_empty(),
                        "invalid publication owner/incarnation at {name}: {publication:?}"
                    );
                    self.unresolved_publications.insert((
                        handle.room_name,
                        publication.participant_id.clone(),
                        publication.id.clone(),
                    ));
                }
            }
            ControlStateOracle::check_client_safety(&observed, &allowed)
                .with_context(|| format!("transition observer={name}"))?;
            for bound in self.bindings.iter().filter(|b| {
                b.crashed
                    && b.retired
                        .is_some_and(|at| at.elapsed() >= CRASH_RETIREMENT_BUDGET)
            }) {
                let track = bound
                    .expected
                    .track_id(pulsebeam::entity::TrackKind::Video, "video")
                    .as_str();
                ensure!(
                    !observed.snapshot.publications.contains_key(&track),
                    "crashed publication exceeded client retirement bound at {name}: {track}"
                );
                if !self
                    .bindings
                    .iter()
                    .any(|b| b.expected.id == bound.expected.id && b.retired.is_none())
                {
                    ensure!(
                        !observed
                            .snapshot
                            .participants
                            .contains_key(&bound.expected.id.as_str()),
                        "crashed participant exceeded client retirement bound at {name}"
                    );
                }
            }
            // Catalog removal is terminal within one admission. A rebuilt client's fresh
            // Catalog has a new source key, unlike independently lagging shard imports.
            self.causal(
                format!("client:{name}:{:?}", observed.admission),
                observed.snapshot.publications.keys().cloned().collect(),
                None,
            )?;
        }
        for (index, since) in &self.holds {
            ensure!(
                since.elapsed() <= Duration::from_secs(5),
                "link hold exceeded five seconds: {index}"
            );
        }
        Ok(())
    }

    async fn owners_safe(&mut self, observer: &pulsebeam::sim_observation::Observer) -> Result<()> {
        let owners = tokio::time::timeout(Duration::from_secs(1), observer.observe())
            .await
            .context("transition owner observation deadline")?
            .map_err(anyhow::Error::msg)?;
        for bound in self
            .bindings
            .iter_mut()
            .filter(|b| b.retired.is_some() && b.retirement_generation.is_none())
        {
            if !owners.control.participants.iter().any(|p| {
                p.id == bound.expected.id && Some(p.connection) == bound.expected.connection
            }) {
                bound.retirement_generation = Some(owners.control.generation);
            }
        }
        let known = self
            .bindings
            .iter()
            .map(|b| (b.expected.id, &b.expected))
            .collect::<BTreeMap<_, _>>();
        for p in &owners.control.participants {
            let expected = known
                .get(&p.id)
                .ok_or_else(|| anyhow::anyhow!("unplanned control admission: {p:?}"))?;
            ensure!(
                self.bindings
                    .iter()
                    .any(|b| b.expected.id == p.id && b.expected.connection == Some(p.connection))
                    && p.room == expected.room_id()?,
                "invalid control generation or room: {p:?}"
            );
        }
        for track in &owners.control.tracks {
            let expected = known
                .get(&track.meta.origin)
                .ok_or_else(|| anyhow::anyhow!("unplanned publication: {:?}", track.meta))?;
            ensure!(
                track.meta.room_id == expected.room_id()?
                    && self
                        .bindings
                        .iter()
                        .any(|b| b.expected.id == track.meta.origin
                            && b.expected.connection == Some(track.meta.connection_id)
                            && b.expected
                                .track_id(pulsebeam::entity::TrackKind::Video, "video")
                                == track.meta.id),
                "foreign-room track"
            );
            for destination in track.candidates.iter().chain(&track.active) {
                ensure!(
                    known
                        .get(destination)
                        .is_some_and(|peer| peer.room == expected.room),
                    "foreign-room forwarding origin"
                );
            }
        }
        let control = owners
            .control
            .participants
            .iter()
            .map(|p| format!("{}:{:?}", p.id.as_str(), p.connection))
            .chain(owners.control.tracks.iter().map(|t| t.meta.id.as_str()))
            .collect();
        self.causal("control".into(), control, Some(owners.control.generation))?;
        for shard in &owners.shards {
            for p in &shard.participants {
                let expected = self
                    .bindings
                    .iter()
                    .find(|b| b.expected.id == p.id && b.expected.connection == Some(p.connection))
                    .ok_or_else(|| anyhow::anyhow!("unplanned shard participant"))?;
                ensure!(
                    Some(p.connection) == expected.expected.connection,
                    "invalid shard connection generation"
                );
            }
            for participant in &shard.participants {
                let destination = self
                    .bindings
                    .iter()
                    .find(|b| {
                        b.expected.id == participant.id
                            && b.expected.connection == Some(participant.connection)
                    })
                    .context("unbound shard destination")?;
                for slot in &participant.video.slots {
                    let source = self
                        .publication(slot.track)
                        .context("unbound shard video assignment")?;
                    ensure!(
                        source.expected.id == slot.origin
                            && source.expected.room == destination.expected.room,
                        "invalid current shard video origin/room"
                    );
                }
            }
            for track in &shard.tracks {
                ensure!(
                    self.publication(*track).is_some(),
                    "unbound shard track: {track}"
                );
            }
            for plan in &shard.plans {
                if let Some(track) = plan.track {
                    let source = self
                        .publication(track)
                        .context("unbound shard plan track")?;
                    for destination in plan.local.iter().flatten() {
                        ensure!(
                            self.bindings.iter().any(|b| b.expected.id == *destination
                                && b.expected.room == source.expected.room),
                            "foreign-room local plan destination"
                        );
                    }
                }
            }
            for route in &shard.routes {
                if let Some(track) = route.track {
                    ensure!(
                        self.publication(track).is_some(),
                        "unbound shard route track"
                    );
                }
            }
            if shard.pending_generation.is_none() && shard.pending_effects == 0 {
                self.causal(
                    format!("shard:{:?}", shard.shard),
                    shard
                        .participants
                        .iter()
                        .map(|p| format!("{}:{:?}", p.id.as_str(), p.connection))
                        .chain(shard.tracks.iter().map(pulsebeam::entity::TrackId::as_str))
                        .chain(
                            shard
                                .participants
                                .iter()
                                .flat_map(|p| p.video.slots.iter().map(|slot| slot.track.as_str())),
                        )
                        .collect(),
                    Some(shard.generation),
                )?;
            }
        }
        // The retirement deadline is anchored to the crash, never to the last failed checkpoint.
        for bound in self.bindings.iter().filter(|b| {
            b.crashed
                && b.retired
                    .is_some_and(|at| at.elapsed() >= CRASH_RETIREMENT_BUDGET)
        }) {
            let id = bound.expected.id;
            let track = bound
                .expected
                .track_id(pulsebeam::entity::TrackKind::Video, "video");
            ensure!(
                !owners
                    .control
                    .participants
                    .iter()
                    .any(|p| p.id == id && Some(p.connection) == bound.expected.connection)
                    && !owners.control.tracks.iter().any(|t| t.meta.id == track)
                    && !owners
                        .shards
                        .iter()
                        .any(|s| s.participants.iter().any(|p| p.id == id
                            && Some(p.connection) == bound.expected.connection
                            || p.video.slots.iter().any(|slot| slot.track == track))
                            || s.tracks.contains(&track)
                            || s.plans.iter().any(|p| p.track == Some(track))
                            || s.routes.iter().any(|r| r.track == Some(track)))
                    && !owners.control.placements.iter().any(|p| p.track == track)
                    && !owners
                        .control
                        .subscriptions
                        .iter()
                        .any(|s| s.selector.track == Some(track)),
                "crashed generation exceeded retirement bound: {id}"
            );
        }
        Ok(())
    }

    async fn wait(&mut self, handles: &PlanHandles, duration: Duration) -> Result<()> {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            self.clients_safe(handles)?;
            tokio::time::sleep(
                Duration::from_millis(10).min(until.saturating_duration_since(Instant::now())),
            )
            .await;
        }
        self.clients_safe(handles)
    }

    async fn admitted(
        &mut self,
        handles: &PlanHandles,
        who: &'static str,
        previous: Option<pulsebeam::entity::ConnectionId>,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                self.clients_safe(handles)?;
                if handles[who].connected()
                    && observation(&handles[who])
                        .is_some_and(|o| o.admission.is_some() && o.admission != previous)
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("admission did not complete")??;
        self.bind(handles)
    }

    async fn receive(
        &mut self,
        handles: &mut PlanHandles,
        who: &'static str,
        targets: Vec<String>,
        repeated_tracks: Option<Vec<String>>,
        witness: &mut Witness,
    ) -> Result<()> {
        let handle = handles.get_mut(who).expect("history receiver");
        let before = observation(handle).context("receiver has no admission observation")?;
        let tracks = if let Some(tracks) = repeated_tracks {
            tracks
        } else {
            targets
                .iter()
                .map(|id| {
                    self.bindings
                        .iter()
                        .rev()
                        .find(|b| b.expected.id.as_str() == *id && b.retired.is_none())
                        .map(|b| {
                            b.expected
                                .track_id(pulsebeam::entity::TrackKind::Video, "video")
                                .as_str()
                        })
                        .context("issued receive target has no live incarnation")
                })
                .collect::<Result<Vec<_>>>()?
        };
        self.outstanding_changes
            .retain(|(_, baseline)| before.snapshot.accepted_intent_revision <= *baseline);
        let predecessor = self
            .outstanding_changes
            .iter()
            .copied()
            .find(|(_, baseline)| {
                before.sent_intent_revision.is_some_and(|sent| {
                    sent > *baseline && sent > before.snapshot.accepted_intent_revision
                })
            });
        self.recovery_at = self.recovery_at.max(Instant::now());
        let command =
            handle
                .shared
                .pending_ops
                .lock()
                .unwrap()
                .push(PendingDriverOp::SetSubscriptions(
                    targets
                        .iter()
                        .zip(&tracks)
                        .map(|(id, track)| VideoSubscription {
                            participant_id: id.clone(),
                            publication: Some(track.clone()),
                            height: 180,
                            min_height: 0,
                            priority: 0,
                        })
                        .collect(),
                ));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                self.clients_safe(handles)?;
                if let Some(after) = observation(&handles[who]) && after.video_operation == command {
                    if who == "viewer" {
                        self.desired = targets.clone();
                        self.desired_tracks = tracks.clone();
                        if self.holds.contains_key(&0) && predecessor.is_some_and(|(prior, _)| prior < command) && before.accepted.is_none() && after.submitted.revision > before.submitted.revision && after.accepted.is_none() {
                            witness.reach("outstanding-replacement");
                            if targets.is_empty() { witness.reach("outstanding-withdrawal"); }
                        }
                        if self.stall.is_some_and(|until| Instant::now() < until) && after.submitted.revision > before.submitted.revision {
                            witness.reach("issued-during-consumed-stall");
                        }
                    }
                    if who == "viewer" && after.submitted.revision > before.submitted.revision {
                        self.outstanding_changes.push((command, before.sent_intent_revision.unwrap_or(before.snapshot.accepted_intent_revision)));
                    }
                    witness.record(format!("issued receiver={who} command={command} predecessor={predecessor:?} app_revision={} wire_revision={:?} accepted_revision={} accepted={} targets={targets:?}", after.submitted.revision, after.sent_intent_revision, after.snapshot.accepted_intent_revision, after.accepted.is_some()));
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.context("receive operation was not actually issued")??;
        Ok(())
    }
}

fn decoded(handles: &PlanHandles, receiver: &str, origin: &str) -> u64 {
    handles[receiver]
        .video_rx()
        .decoded_from(origin)
        .map_or(0, |s| s.frames)
}

pub(super) async fn execute(
    history: &History,
    shared: &Mutex<Witness>,
    handles: &mut PlanHandles,
    ips: &PlanIps,
    observer: &pulsebeam::sim_observation::Observer,
) -> Result<()> {
    let mut witness = shared.lock().unwrap().clone();
    use futures::FutureExt;
    let result =
        std::panic::AssertUnwindSafe(execute_inner(history, &mut witness, handles, ips, observer))
            .catch_unwind()
            .await;
    for op in &history.ops {
        if let Op::Hold(i) = op {
            turmoil::release(ips[name(*i)], ips["server"]);
        }
    }
    *shared.lock().unwrap() = witness;
    match result {
        Ok(result) => result,
        Err(error) => std::panic::resume_unwind(error),
    }
}

async fn execute_inner(
    history: &History,
    witness: &mut Witness,
    handles: &mut PlanHandles,
    ips: &PlanIps,
    observer: &pulsebeam::sim_observation::Observer,
) -> Result<()> {
    witness.phase = "establish".into();
    let mut state = Transition {
        bindings: Vec::new(),
        seen: BTreeMap::new(),
        tombstones: BTreeMap::new(),
        holds: BTreeMap::new(),
        desired: Vec::new(),
        desired_tracks: Vec::new(),
        outstanding_changes: Vec::new(),
        admitting: BTreeSet::new(),
        unresolved: BTreeSet::new(),
        unresolved_publications: BTreeSet::new(),
        stall: None,
        stall_count: pulsebeam::sim_metrics::controller_stalls_consumed(),
        failed_count: pulsebeam::sim_metrics::routing_counter("materialization_failed"),
        orphan_count: pulsebeam::sim_metrics::routing_counter("materialization_orphan"),
        failed: false,
        recovery_at: Instant::now(),
    };
    state.bind(handles)?;
    let anchor = handles["anchor"]
        .participant_id()
        .context("anchor admission")?;
    state
        .receive(handles, "viewer", vec![anchor.clone()], None, witness)
        .await?;
    state
        .receive(handles, "continuing", vec![anchor.clone()], None, witness)
        .await?;
    recover(&mut state, handles, observer, false).await?;
    let mut continuing_before = decoded(handles, "continuing", &anchor);
    let mut held_tx = BTreeMap::new();
    for (index, op) in history.ops.iter().enumerate() {
        witness.phase = format!("operation:{op:?}");
        witness.record(format!("operation={index} program={op:?}"));
        match op {
            Op::Join(i) | Op::Readmit(i) => {
                let who = CAST[usize::from(*i)];
                state.admitting.insert(who);
                let handle = handles.get_mut(who).unwrap();
                let previous = observation(handle).and_then(|o| o.admission);
                handle.shared.pending_ops.lock().unwrap().restart();
                handle.present = true;
                state.recovery_at = state.recovery_at.max(Instant::now());
                handle.send_command(ParticipantCmd::Reconnect);
                if !state.holds.contains_key(&(*i + 1)) {
                    state.admitted(handles, who, previous).await?;
                    let id = handles[who]
                        .participant_id()
                        .context("admitted publisher")?;
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while !observation(&handles["viewer"]).is_some_and(|o| {
                            o.snapshot
                                .publications
                                .values()
                                .any(|p| p.participant_id == id)
                        }) {
                            state.wait(handles, Duration::from_millis(10)).await?;
                        }
                        Ok::<_, anyhow::Error>(())
                    })
                    .await
                    .context("new publication not observed before next operation")??;
                } else {
                    witness.record(format!(
                        "readmission-command previous={previous:?} link-held=true"
                    ));
                }
            }
            Op::Leave(i) | Op::Crash(i) => {
                let who = CAST[usize::from(*i)];
                let handle = handles.get_mut(who).unwrap();
                let id = handle
                    .participant_id()
                    .context("retire admitted identity")?;
                let bound = state
                    .bindings
                    .iter_mut()
                    .rev()
                    .find(|b| b.expected.id.as_str() == id && b.retired.is_none())
                    .context("independent admission binding")?;
                bound.crashed = matches!(op, Op::Crash(_));
                state.recovery_at = state.recovery_at.max(Instant::now());
                bound.retired = Some(Instant::now());
                handle.present = false;
                handle.retirement_deadline =
                    matches!(op, Op::Crash(_)).then(|| Instant::now() + CRASH_RETIREMENT_BUDGET);
                handle.shared.pending_ops.lock().unwrap().restart();
                if let Some(tx) = held_tx.get(&(*i + 1)) {
                    ensure!(
                        handle.tx_bytes() > *tx,
                        "no previous-generation traffic produced while link held"
                    );
                    witness.reach("previous-generation-in-flight");
                }
                handle.send_command(if matches!(op, Op::Crash(_)) {
                    ParticipantCmd::Drop
                } else {
                    ParticipantCmd::Shutdown
                });
                tokio::time::timeout(Duration::from_secs(2), async {
                    while handles[who].connected() {
                        state.wait(handles, Duration::from_millis(10)).await?;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("retirement command not applied")??;
            }
            Op::Receive(targets) => {
                // Wait for catalogs to contain independently admitted origins before issuing intent.
                let targets = targets
                    .iter()
                    .map(|i| {
                        if *i == 0 {
                            Ok(anchor.clone())
                        } else {
                            handles[CAST[usize::from(*i - 1)]]
                                .participant_id()
                                .context("receive target admission")
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if observation(&handles["viewer"]).is_some_and(|o| {
                            targets.iter().all(|id| {
                                o.snapshot
                                    .publications
                                    .values()
                                    .any(|p| p.participant_id == *id)
                            })
                        }) {
                            return Ok::<_, anyhow::Error>(());
                        }
                        state.wait(handles, Duration::from_millis(10)).await?;
                    }
                })
                .await
                .context("target publication not discovered")??;
                state
                    .receive(handles, "viewer", targets, None, witness)
                    .await?;
            }
            Op::Repeat => {
                let desired = state.desired.clone();
                let tracks = state.desired_tracks.clone();
                state
                    .receive(handles, "viewer", desired, Some(tracks), witness)
                    .await?;
            }
            Op::Hold(i) => {
                let who = name(*i);
                state.holds.insert(*i, Instant::now());
                held_tx.insert(*i, handles[who].tx_bytes());
                turmoil::hold(ips[who], ips["server"]);
            }
            Op::Release(i) => {
                let who = name(*i);
                turmoil::release(ips[who], ips["server"]);
                state.holds.remove(i);
                held_tx.remove(i);
                state.recovery_at = state.recovery_at.max(Instant::now());
                if *i > 0 && state.admitting.contains(who) {
                    let previous = state
                        .bindings
                        .iter()
                        .rev()
                        .find(|b| b.name == who)
                        .and_then(|b| b.expected.connection);
                    state.admitted(handles, who, previous).await?;
                    witness.reach("explicit-readmission");
                }
            }
            Op::Pause(ms) => {
                state
                    .wait(handles, Duration::from_millis(u64::from(*ms)))
                    .await?;
            }
            Op::Stall => {
                let before = pulsebeam::sim_metrics::controller_stalls_consumed();
                pulsebeam::sim_metrics::request_controller_stall(Duration::from_secs(5));
                state.stall = Some(Instant::now() + Duration::from_secs(5));
                state.recovery_at = state.recovery_at.max(state.stall.unwrap());
                tokio::time::timeout(Duration::from_secs(1), async {
                    while pulsebeam::sim_metrics::controller_stalls_consumed() == before {
                        state.wait(handles, Duration::from_millis(10)).await?;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("controller stall not consumed")??;
                ensure!(
                    pulsebeam::sim_metrics::controller_stalls_consumed() - before == 1,
                    "stall consumption delta"
                );
                continuing_before = decoded(handles, "continuing", &anchor);
                witness.reach("controller-stall-consumed-once");
            }
            Op::FailAdmission => {
                *handles["retry"]
                    .shared
                    .driver_admission_retry
                    .lock()
                    .unwrap() = true;
                let before = *handles["retry"].shared.failed_admissions.lock().unwrap();
                pulsebeam::sim_metrics::fail_next_materialization();
                handles["retry"].send_command(ParticipantCmd::Reconnect);
                tokio::time::timeout(Duration::from_secs(5), async {
                    while pulsebeam::sim_metrics::routing_counter("materialization_failed")
                        == state.failed_count
                        || *handles["retry"].shared.failed_admissions.lock().unwrap() == before
                    {
                        state.wait(handles, Duration::from_millis(10)).await?;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("materialization fault not consumed")??;
                ensure!(
                    pulsebeam::sim_metrics::routing_counter("materialization_failed")
                        - state.failed_count
                        == 1,
                    "materialization consumption delta"
                );
                state.failed = true;
                witness.reach("materialization-consumed-once");
                state.owners_safe(observer).await?;
                ensure!(
                    !handles["retry"].connected() && handles["retry"].participant_id().is_none(),
                    "failed materialization admitted a client"
                );
                ensure!(
                    pulsebeam::sim_metrics::routing_counter("materialization_orphan")
                        == state.orphan_count,
                    "materialization orphan"
                );
                witness.reach("no-materialization-orphan");
            }
            Op::Retry => {
                ensure!(state.failed, "retry requires consumed failure");
                state.admitting.insert("retry");
                let handle = handles.get_mut("retry").unwrap();
                handle.present = true;
                state.recovery_at = state.recovery_at.max(Instant::now());
                handle.send_command(ParticipantCmd::Reconnect);
                state.admitted(handles, "retry", None).await?;
                state
                    .receive(handles, "retry", vec![anchor.clone()], None, witness)
                    .await?;
                witness.reach("planned-admission-retry");
            }
        }
        witness.applied += 1;
        state.clients_safe(handles)?;
        if state.holds.is_empty() && state.stall.is_none_or(|until| Instant::now() >= until) {
            state.owners_safe(observer).await?;
        }
        if state.stall.is_some_and(|until| Instant::now() < until)
            && decoded(handles, "continuing", &anchor) > continuing_before
        {
            witness.reach("unrelated-fresh-during-stall");
        }
    }
    witness.phase = "required-witness".into();
    ensure!(state.holds.is_empty(), "history did not release its links");
    if let Some(until) = state.stall {
        state
            .wait(handles, until.saturating_duration_since(Instant::now()))
            .await?;
    }
    let stalls = pulsebeam::sim_metrics::controller_stalls_consumed() - state.stall_count;
    let failures =
        pulsebeam::sim_metrics::routing_counter("materialization_failed") - state.failed_count;
    ensure!(
        stalls == u64::from(history.claim == Claim::Stall),
        "per-history stall consumption mismatch"
    );
    ensure!(
        failures == u64::from(history.claim == Claim::Materialization),
        "per-history materialization consumption mismatch"
    );
    let required: &[&str] = match history.claim {
        Claim::Exploration => &[],
        Claim::Replacement => &["outstanding-replacement", "outstanding-withdrawal"],
        Claim::Generation => &["previous-generation-in-flight", "explicit-readmission"],
        Claim::Stall => &[
            "controller-stall-consumed-once",
            "issued-during-consumed-stall",
            "unrelated-fresh-during-stall",
        ],
        Claim::Materialization => &[
            "materialization-consumed-once",
            "no-materialization-orphan",
            "planned-admission-retry",
        ],
    };
    ensure!(
        required.iter().all(|s| witness.reached.contains(*s)),
        "missed required witness: required={required:?} reached={:?}",
        witness.reached
    );
    witness.phase = "recovery".into();
    recover(&mut state, handles, observer, true).await?;
    witness.reach("converged-fresh-all-receivers-and-bystander");
    Ok(())
}

async fn recover(
    state: &mut Transition,
    handles: &PlanHandles,
    observer: &pulsebeam::sim_observation::Observer,
    final_check: bool,
) -> Result<()> {
    // Baselines are taken after the later of repair and the last recovery command.
    let started = Instant::now();
    let mut intended = vec![
        ("continuing", handles["anchor"].participant_id().unwrap()),
        ("bystander", handles["foreign"].participant_id().unwrap()),
    ];
    intended.extend(
        state
            .desired
            .iter()
            .filter(|id| {
                state.bindings.iter().any(|b| {
                    b.expected.id.as_str() == **id
                        && b.retired.is_none()
                        && state.desired_tracks.contains(
                            &b.expected
                                .track_id(pulsebeam::entity::TrackKind::Video, "video")
                                .as_str(),
                        )
                })
            })
            .cloned()
            .map(|id| ("viewer", id)),
    );
    if handles["retry"].present {
        intended.push(("retry", handles["anchor"].participant_id().unwrap()));
    }
    // Withdrawal is valid, but the final positive claim must not be vacuous.
    ensure!(!intended.is_empty(), "empty final media oracle");
    let before = intended
        .iter()
        .map(|(receiver, id)| decoded(handles, receiver, id))
        .collect::<Vec<_>>();
    let expected_tracks = state
        .desired_tracks
        .iter()
        .filter(|track| {
            state.bindings.iter().any(|b| {
                b.retired.is_none()
                    && b.expected.video
                    && b.expected
                        .track_id(pulsebeam::entity::TrackKind::Video, "video")
                        .as_str()
                        == **track
            })
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut last = None;
    let budget = if final_check {
        (state.recovery_at + Duration::from_secs(30)).saturating_duration_since(Instant::now())
    } else {
        Duration::from_secs(30)
    };
    ensure!(
        !budget.is_zero(),
        "recovery deadline exhausted before final checkpoint"
    );
    tokio::time::timeout(budget, async {
        loop {
            state.clients_safe(handles)?;
            state.owners_safe(observer).await?;
            match assert_convergence(handles, observer, "intent history recovery").await {
                Ok(())
                    if (!final_check
                        || observation(&handles["viewer"])
                            .and_then(|o| o.accepted)
                            .is_some_and(|intent| {
                                intent
                                    .video
                                    .into_iter()
                                    .map(|v| v.track_id)
                                    .collect::<BTreeSet<_>>()
                                    == expected_tracks
                            }))
                        && intended.iter().zip(&before).all(|((receiver, id), count)| {
                            handles[*receiver]
                                .video_rx()
                                .require_decoded_from(id, *count, 1)
                                .is_ok()
                        }) =>
                {
                    break Ok::<_, anyhow::Error>(());
                }
                Ok(()) => {}
                Err(error) => last = Some(error),
            }
            state.wait(handles, Duration::from_millis(20)).await?;
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "history did not converge with fresh intended origins in 30 seconds: {last:?}"
        )
    })??;
    // No client in either room may render an origin from the other room, including old intervals.
    for (name, handle) in handles.iter().filter(|(_, h)| h.present) {
        for bound in state
            .bindings
            .iter()
            .filter(|b| b.expected.room != handle.room_name)
        {
            ensure!(
                handle
                    .video_rx()
                    .require_not_received_from(&bound.expected.id.as_str())
                    .is_ok(),
                "foreign-room decoded origin at {name}"
            );
        }
    }
    if final_check {
        let actual = handles["viewer"]
            .shared
            .pending_ops
            .lock()
            .unwrap()
            .intent
            .video
            .iter()
            .map(|v| v.participant_id.clone())
            .collect::<Vec<_>>();
        ensure!(actual == state.desired, "latest semantic intent lost");
        let expected_tracks = state
            .desired_tracks
            .iter()
            .filter(|track| {
                state.bindings.iter().any(|b| {
                    b.retired.is_none()
                        && b.expected.video
                        && b.expected
                            .track_id(pulsebeam::entity::TrackKind::Video, "video")
                            .as_str()
                            == **track
                })
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        let observed = observation(&handles["viewer"]).context("final receiver observation")?;
        let accepted_tracks = observed
            .accepted
            .context("final intent not accepted")?
            .video
            .into_iter()
            .map(|v| v.track_id)
            .collect::<BTreeSet<_>>();
        ensure!(
            accepted_tracks == expected_tracks,
            "latest incarnation-specific intent lost: expected={expected_tracks:?}, accepted={accepted_tracks:?}"
        );
        println!(
            "[history-recovery] simulated_ms={} intended={intended:?}",
            started.elapsed().as_millis()
        );
    }
    Ok(())
}
