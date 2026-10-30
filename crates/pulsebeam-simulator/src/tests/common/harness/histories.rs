use super::super::oracle::{ApplicationStateOracle, ExpectedParticipant};
use super::*;
use crate::tests::histories::{CAST, Claim, Delivery, History, Op, Witness};
use anyhow::{Context, Result, ensure};

mod data;
use data::DataLane;
mod audio;

struct Bound {
    name: &'static str,
    expected: ExpectedParticipant,
    retired: Option<Instant>,
    crashed: bool,
}

struct HeldNativeMedia {
    participant_id: String,
    admission: pulsebeam::entity::ConnectionId,
    held_at_us: i64,
    baseline: NativeTrafficStatistics,
}

struct Transition {
    bindings: Vec<Bound>,
    seen: BTreeMap<String, BTreeSet<String>>,
    tombstones: BTreeMap<String, BTreeSet<String>>,
    holds: BTreeMap<u8, Instant>,
    desired: Vec<String>,
    desired_tracks: Vec<String>,
    continuing_logical: bool,
    admitting: BTreeSet<&'static str>,
    unresolved: BTreeSet<(&'static str, String)>,
    unresolved_publications: BTreeSet<(&'static str, String, String)>,
    stall: Option<Instant>,
    stall_receipts: u64,
    failed_receipts: u64,
    failed: bool,
    recovery_at: Instant,
}

fn publication_ids(expected: &ExpectedParticipant) -> BTreeSet<String> {
    [
        (expected.video, pulsebeam::entity::TrackKind::Video, "video"),
        (expected.audio, pulsebeam::entity::TrackKind::Audio, "audio"),
    ]
    .into_iter()
    .filter(|(present, _, _)| *present)
    .map(|(_, kind, label)| expected.track_id(kind, label).as_str())
    .collect()
}

fn observation(handle: &ParticipantHandle) -> Option<ClientObservation> {
    handle.shared.observation.lock().unwrap().clone()
}

fn requested_tracks(observed: &ClientObservation) -> BTreeSet<String> {
    observed
        .submitted
        .video
        .iter()
        .map(|video| video.track_id.clone())
        .collect()
}

fn mapped_tracks(observed: &ClientObservation) -> BTreeSet<String> {
    observed.snapshot.video_mapping.values().cloned().collect()
}

fn request_changes(
    previous_requested: &BTreeSet<String>,
    previous_mapped: &BTreeSet<String>,
    requested: &BTreeSet<String>,
    mapped: &BTreeSet<String>,
) -> (bool, bool) {
    let changed = requested != previous_requested;
    (
        changed && previous_requested != previous_mapped,
        changed && requested.is_empty() && !mapped.is_empty(),
    )
}

fn fault_target(index: u8) -> &'static str {
    if index == 0 {
        "viewer"
    } else {
        CAST[usize::from(index - 1)]
    }
}

impl Transition {
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
            let intent = handle.shared.pending_ops.lock().unwrap().intent.clone();
            let expected = ExpectedParticipant {
                id: id.parse()?,
                room: handle.room_name,
                video: handle.publishes_video,
                video_label: handle.shared.video_label.lock().unwrap().clone(),
                audio: handle.publishes_audio,
                audio_pinned: intent.audio,
                automatic_audio: intent.automatic_audio,
                required: true,
                connection: Some(connection),
                topics: intent.topics,
            };
            self.bindings.push(Bound {
                name,
                expected,
                retired: None,
                crashed: false,
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
                        && publication_ids(&b.expected).contains(track)
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

    fn causal(&mut self, source: String, present: BTreeSet<String>) -> Result<()> {
        let retired = self
            .bindings
            .iter()
            .filter(|b| b.retired.is_some())
            .flat_map(|b| publication_ids(&b.expected))
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
                let (kind, label) = match publication.kind {
                    pulsebeam_agent_native::agent_core::MediaKind::Video => {
                        (pulsebeam::entity::TrackKind::Video, "video")
                    }
                    pulsebeam_agent_native::agent_core::MediaKind::Audio => {
                        (pulsebeam::entity::TrackKind::Audio, "audio")
                    }
                };
                ensure!(
                    publication.label == label,
                    "unexpected history publication label"
                );
                if !self.bindings.iter().any(|b| {
                    b.expected.room == handle.room_name
                        && b.expected.id.as_str() == publication.participant_id
                        && (match publication.kind {
                            pulsebeam_agent_native::agent_core::MediaKind::Video => {
                                b.expected.video
                            }
                            pulsebeam_agent_native::agent_core::MediaKind::Audio => {
                                b.expected.audio
                            }
                        })
                        && b.expected.track_id(kind, label).as_str() == publication.id
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
            ApplicationStateOracle::check_client_safety(&observed, &allowed)
                .with_context(|| format!("transition observer={name}"))?;
            for bound in self.bindings.iter().filter(|b| {
                b.crashed
                    && b.retired
                        .is_some_and(|at| at.elapsed() >= CRASH_RETIREMENT_BUDGET)
            }) {
                for track in publication_ids(&bound.expected) {
                    ensure!(
                        !observed.snapshot.publications.contains_key(&track),
                        "crashed publication exceeded client retirement bound at {name}: {track}"
                    );
                }
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
        let previous_requested = requested_tracks(&before);
        let previous_mapped = mapped_tracks(&before);
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
                            // The continuing call follows its logical publisher across
                            // legitimate transport replacement. Viewer requests pin lifetimes.
                            publication: (who != "continuing" || !self.continuing_logical)
                                .then(|| track.clone()),
                            height: 180,
                            ..Default::default()
                        })
                        .collect(),
                ));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                self.clients_safe(handles)?;
                if let Some(after) = observation(&handles[who]) && after.video_operation == command {
                    let requested = requested_tracks(&after);
                    let mapped = mapped_tracks(&after);
                    if who == "viewer" {
                        self.desired = targets.clone();
                        self.desired_tracks = tracks.clone();
                        if self.holds.contains_key(&0) {
                            let (replaces_pending, withdraws_pending) = request_changes(
                                &previous_requested, &previous_mapped, &requested, &mapped,
                            );
                            if replaces_pending {
                                witness.reach("outstanding-replacement");
                            }
                            if withdraws_pending {
                                witness.reach("outstanding-withdrawal");
                            }
                        }
                        if self.stall.is_some_and(|until| Instant::now() < until) && after.submitted.revision > before.submitted.revision {
                            witness.reach("issued-during-consumed-stall");
                        }
                    }
                    witness.record(format!("issued receiver={who} command={command} app_revision={} previous_requested={previous_requested:?} previous_mapped={previous_mapped:?} requested={requested:?} mapped={mapped:?} targets={targets:?}", after.submitted.revision));
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
) -> Result<()> {
    let mut witness = shared.lock().unwrap().clone();
    use futures::FutureExt;
    let result = std::panic::AssertUnwindSafe(execute_inner(history, &mut witness, handles, ips))
        .catch_unwind()
        .await;
    for op in &history.ops {
        if let Op::Hold(i) = op {
            turmoil::release(ips[fault_target(*i)], ips["server"]);
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
) -> Result<()> {
    witness.phase = "establish".into();
    let mut state = Transition {
        bindings: Vec::new(),
        seen: BTreeMap::new(),
        tombstones: BTreeMap::new(),
        holds: BTreeMap::new(),
        desired: Vec::new(),
        desired_tracks: Vec::new(),
        continuing_logical: history
            .ops
            .iter()
            .any(|op| matches!(op, Op::DeclareData(_))),
        admitting: BTreeSet::new(),
        unresolved: BTreeSet::new(),
        unresolved_publications: BTreeSet::new(),
        stall: None,
        stall_receipts: 0,
        failed_receipts: 0,
        failed: false,
        recovery_at: Instant::now(),
    };
    for who in ["anchor", "viewer", "continuing", "foreign", "bystander"] {
        state.admitted(handles, who, None).await?;
    }
    let anchor = handles["anchor"]
        .participant_id()
        .context("anchor admission")?;
    state
        .receive(handles, "viewer", vec![anchor.clone()], None, witness)
        .await?;
    state
        .receive(handles, "continuing", vec![anchor.clone()], None, witness)
        .await?;
    recover(&mut state, handles, false).await?;
    let mut continuing_before = decoded(handles, "continuing", &anchor);
    let mut held_media = BTreeMap::<u8, HeldNativeMedia>::new();
    let mut data_lanes = BTreeMap::<Delivery, DataLane>::new();
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
                let id = handles[who]
                    .participant_id()
                    .context("retire admitted identity")?;
                if let Some(held) = held_media.remove(&(*i + 1)) {
                    let before = observation(&handles[who]).context("held source observation")?;
                    ensure!(
                        held.participant_id == id
                            && before.admission == Some(held.admission)
                            && before
                                .snapshot
                                .generation
                                .map(pulsebeam_agent_native::agent_core::Generation::get)
                                == Some(held.baseline.generation),
                        "held media source changed admission or native generation before retirement"
                    );
                    let mut pending = None;
                    let ending = tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            let sample = handles[who].shared.statistics.lock().unwrap().clone();
                            if let Some(sample) = sample {
                                ensure!(sample.generation == held.baseline.generation,
                                    "held media native generation changed before retirement");
                                match sample.video_progress_since(&held.baseline, "v0") {
                                    Ok(true) => break Ok::<_, anyhow::Error>(sample),
                                    Ok(false) => pending = Some("old video sender has not emitted another frame".to_owned()),
                                    Err(reason) => pending = Some(reason),
                                }
                            }
                            state.wait(handles, Duration::from_millis(10)).await?;
                        }
                    }).await.with_context(|| format!("no actual previous-generation video RTP produced while held: {pending:?}"))??;
                    let binding = state
                        .bindings
                        .iter()
                        .rev()
                        .find(|binding| {
                            binding.expected.id.as_str() == id && binding.retired.is_none()
                        })
                        .context("held source admission binding")?;
                    witness.record(format!("held-media participant={id} admission={} publication={} native_generation={} held_at_us={} baseline_us={} ending_us={} old_senders={:?} ending_senders={:?}",
                        held.admission, binding.expected.track_id(pulsebeam::entity::TrackKind::Video, "video").as_str(),
                        held.baseline.generation, held.held_at_us, held.baseline.timestamp_us, ending.timestamp_us,
                        held.baseline.outbound_video, ending.outbound_video));
                    witness.reach("previous-generation-in-flight");
                }
                // Retire only after the real old sender's held-media witness.
                // Waiting for evidence must not start the product recovery clock.
                let handle = handles.get_mut(who).unwrap();
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
                let who = fault_target(*i);
                let held_at_us = super::super::native::now_us()?;
                state.holds.insert(*i, Instant::now());
                turmoil::hold(ips[who], ips["server"]);
                if *i > 0 {
                    let before =
                        observation(&handles[who]).context("held publisher observation")?;
                    let generation = before
                        .snapshot
                        .generation
                        .context("held native generation")?
                        .get();
                    let admission = before.admission.context("held source admission")?;
                    let participant_id = handles[who]
                        .participant_id()
                        .context("held source identity")?;
                    let baseline = tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            let sample = handles[who].shared.statistics.lock().unwrap().clone();
                            if let Some(sample) = sample
                                && sample.generation == generation
                                && sample.timestamp_us > held_at_us
                                && sample.has_video_sender("v0")
                            {
                                break Ok::<_, anyhow::Error>(sample);
                            }
                            state.wait(handles, Duration::from_millis(10)).await?;
                        }
                    })
                    .await
                    .context("no fresh native video sender report after hold")??;
                    held_media.insert(
                        *i,
                        HeldNativeMedia {
                            participant_id,
                            admission,
                            held_at_us,
                            baseline,
                        },
                    );
                }
            }
            Op::Release(i) => {
                let who = fault_target(*i);
                turmoil::release(ips[who], ips["server"]);
                state.holds.remove(i);
                held_media.remove(i);
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
            Op::DeclareData(mode) => {
                let lane = DataLane::declare(*mode, &mut state, handles).await?;
                data_lanes.insert(*mode, lane);
                witness.reach(&format!(
                    "topic-{}-established-payload-service",
                    mode.name()
                ));
            }
            Op::ReplaceData(mode) => {
                data_lanes
                    .get_mut(mode)
                    .context("declared data lane")?
                    .replace()
                    .await?;
                witness.reach(&format!("topic-{}-old-owner-released", mode.name()));
            }
            Op::SendData(mode, tag) => {
                let lane = data_lanes.get_mut(mode).context("declared data lane")?;
                lane.send(*tag, &mut state, handles).await?;
                witness.reach(&format!("topic-{}-mode-delivery", mode.name()));
                if lane.replaced {
                    witness.reach(&format!(
                        "topic-{}-fresh-replacement-owner-delivery",
                        mode.name()
                    ));
                    if !state.holds.is_empty()
                        || state.stall.is_some_and(|until| Instant::now() < until)
                    {
                        witness.reach(&format!(
                            "topic-{}-fresh-replacement-owner-delivery-during-pending",
                            mode.name()
                        ));
                    }
                }
            }
            Op::Audio {
                pinned,
                automatic,
                fixed_ms,
            } => {
                audio::request(&mut state, handles, pinned, *automatic, *fixed_ms, witness).await?;
            }
            Op::AudioProgress => audio::progress(&mut state, handles, witness).await?,
            Op::Pressure => audio::pressure(&mut state, handles, witness).await?,
            Op::ReplaceSource(i) | Op::ReleaseSource(i) => {
                let who = CAST[usize::from(*i)];
                let admission = observation(&handles[who])
                    .and_then(|o| o.admission)
                    .context("source operation admission")?;
                let before_source = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        state.clients_safe(handles)?;
                        if let Ok((source, _)) = source_context(handles, "viewer", who)
                            && handles["viewer"]
                                .video_rx()
                                .decoded_from_incarnation(&source)
                                .is_some_and(|decoded| decoded.frames > 0)
                        {
                            return Ok::<_, anyhow::Error>(source);
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .context("source service did not become visible before owner operation")??;
                witness.reach("source-service-established-before-owner-operation");
                let completed = Arc::new(AtomicBool::new(false));
                let operation = if matches!(op, Op::ReplaceSource(_)) {
                    PendingDriverOp::ReplaceVideoSource(completed.clone())
                } else {
                    PendingDriverOp::ReleaseReplacedVideoSource(completed.clone())
                };
                handles[who]
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(operation);
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !completed.load(Ordering::Acquire) {
                        state.clients_safe(handles)?;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("source input operation did not settle")??;
                ensure!(
                    observation(&handles[who]).and_then(|o| o.admission) == Some(admission),
                    "local producer operation replaced its HTTP admission"
                );
                let (source, _) = source_context(handles, "viewer", who)?;
                if matches!(op, Op::ReplaceSource(_)) {
                    ensure!(
                        source != before_source,
                        "replacement reused source/capture namespace"
                    );
                    witness.reach("source-input-replaced-same-admission");
                } else {
                    ensure!(
                        source == before_source,
                        "retired close changed current source construction"
                    );
                    let before = handles["viewer"]
                        .video_rx()
                        .decoded_from_incarnation(&source)
                        .map_or(0, |decoded| decoded.frames);
                    tokio::time::timeout(Duration::from_secs(5), async {
                        loop {
                            state.clients_safe(handles)?;
                            if handles["viewer"]
                                .video_rx()
                                .require_decoded_from_incarnation(&source, before, 1)
                                .is_ok()
                            {
                                return Ok::<_, anyhow::Error>(());
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .context("replacement source did not decode after retired close")??;
                    witness.reach("fresh-replacement-source-after-retired-close");
                }
                state.recovery_at = state.recovery_at.max(Instant::now());
            }
            Op::Pause(ms) => {
                state
                    .wait(handles, Duration::from_millis(u64::from(*ms)))
                    .await?;
            }
            Op::Stall => {
                let (until, mut begun) =
                    pulsebeam::sim_metrics::request_controller_stall(Duration::from_secs(5));
                state.stall = Some(until);
                state.recovery_at = state.recovery_at.max(state.stall.unwrap());
                tokio::time::timeout(Duration::from_secs(1), async {
                    while let Err(error) = begun.try_recv() {
                        ensure!(
                            matches!(error, tokio::sync::oneshot::error::TryRecvError::Empty),
                            "controller stall input cancelled"
                        );
                        state.wait(handles, Duration::from_millis(10)).await?;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("controller stall not consumed")??;
                state.stall_receipts = state.stall_receipts.saturating_add(1);
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
                    while *handles["retry"].shared.failed_admissions.lock().unwrap() == before {
                        state.wait(handles, Duration::from_millis(10)).await?;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .context("application admission did not fail")??;
                ensure!(
                    *handles["retry"].shared.failed_admissions.lock().unwrap() - before == 1,
                    "application admission failure count"
                );
                state.failed_receipts = state.failed_receipts.saturating_add(1);
                state.failed = true;
                witness.reach("application-admission-failed-once");
                ensure!(
                    !handles["retry"].connected() && handles["retry"].participant_id().is_none(),
                    "failed admission exposed a connected identity"
                );
                witness.reach("failed-admission-has-no-identity");
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
    let stalls = state.stall_receipts;
    let failures = state.failed_receipts;
    ensure!(
        stalls == u64::from(history.claim == Claim::Stall),
        "per-history stall input acknowledgment mismatch"
    );
    ensure!(
        failures == u64::from(history.claim == Claim::Materialization),
        "per-history failed application admission mismatch"
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
            "application-admission-failed-once",
            "failed-admission-has-no-identity",
            "planned-admission-retry",
        ],
    };
    ensure!(
        required.iter().all(|s| witness.reached.contains(*s)),
        "missed required witness: required={required:?} reached={:?}",
        witness.reached
    );
    witness.phase = "recovery".into();
    recover(&mut state, handles, true).await?;
    witness.reach("converged-fresh-all-receivers-and-bystander");
    Ok(())
}

async fn recover(state: &mut Transition, handles: &PlanHandles, final_check: bool) -> Result<()> {
    // Baselines are taken after the later of repair and the last recovery command.
    let started = Instant::now();
    let current = state
        .bindings
        .iter()
        .filter(|bound| {
            bound.retired.is_none()
                && observation(&handles[bound.name])
                    .is_some_and(|observed| observed.admission == bound.expected.connection)
        })
        .map(|bound| bound.expected.clone())
        .collect::<Vec<_>>();
    let mut intended = vec![
        ("continuing", handles["anchor"].participant_id().unwrap()),
        ("bystander", handles["foreign"].participant_id().unwrap()),
    ];
    intended.extend(
        state
            .desired
            .iter()
            .filter(|id| {
                current.iter().any(|binding| {
                    binding.id.as_str() == **id
                        && state.desired_tracks.contains(
                            &binding
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
    // Bind freshness to the independent admission and actual caller-created
    // source lifetime, not a reusable logical publisher or restarted capture counter.
    let origins = intended
        .iter()
        .map(|(_, id)| {
            let binding = current
                .iter()
                .find(|binding| binding.id.as_str() == *id)
                .context("intended media current admission binding")?;
            let admission = binding
                .connection
                .context("intended media HTTP admission")?;
            super::super::provenance::current_source_key(
                &handles["anchor"].shared.h264_publishers.lock().unwrap(),
                &admission.to_string(),
                "v0",
            )
            .context("intended media source construction")
        })
        .collect::<Result<Vec<_>>>()?;
    let before = intended
        .iter()
        .zip(&origins)
        .map(|((receiver, _), origin)| {
            handles[*receiver]
                .video_rx()
                .decoded_from_incarnation(origin)
                .map_or(0, |decoded| decoded.frames)
        })
        .collect::<Vec<_>>();
    let expected_tracks = state
        .desired_tracks
        .iter()
        .filter(|track| {
            current.iter().any(|binding| {
                binding.video
                    && binding
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
            match assert_convergence(handles, "intent history recovery").await {
                Ok(())
                    if (!final_check
                        || observation(&handles["viewer"])
                            .is_some_and(|observed| mapped_tracks(&observed) == expected_tracks))
                        && intended.iter().zip(&origins).zip(&before).all(|(((receiver, _), origin), count)| {
                            handles[*receiver].video_rx().require_decoded_from_incarnation(origin, *count, 1).is_ok()
                        }) =>
                {
                    break Ok::<_, anyhow::Error>(());
                }
                Ok(()) => {
                    let progress = intended
                        .iter()
                        .zip(&origins)
                        .zip(&before)
                        .map(|(((receiver, id), origin), count)| {
                            (*receiver, id, origin, *count, handles[*receiver].video_rx().decoded_from_incarnation(origin).map_or(0, |decoded| decoded.frames))
                        })
                        .collect::<Vec<_>>();
                    last = Some(anyhow::anyhow!(
                        "control converged; waiting for intended track acceptance and fresh decoded media: {progress:?}"
                    ));
                }
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
        let observed = observation(&handles["viewer"]).context("final receiver observation")?;
        let mapped = mapped_tracks(&observed);
        ensure!(
            mapped == expected_tracks,
            "latest incarnation-specific request lost: expected={expected_tracks:?}, mapped={mapped:?}"
        );
        println!(
            "[history-recovery] simulated_ms={} intended={intended:?}",
            started.elapsed().as_millis()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::request_changes;
    use std::collections::BTreeSet;

    #[test]
    fn request_claims_allow_cancellation_to_visible_state_and_idempotence() {
        let tracks = |names: &[&str]| {
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<BTreeSet<_>>()
        };
        for (before, mapped, after, expected) in [
            (vec!["y"], vec!["x"], vec!["x"], (true, false)),
            (vec!["x"], vec!["x"], vec![], (false, true)),
            (vec!["y"], vec!["x"], vec![], (true, true)),
            (vec![], vec!["x"], vec![], (false, false)),
            (vec!["x"], vec!["x"], vec!["y"], (false, false)),
            (vec!["y"], vec!["x"], vec!["y"], (false, false)),
            (vec!["y"], vec!["x"], vec!["z"], (true, false)),
            (vec![], vec![], vec![], (false, false)),
        ] {
            assert_eq!(
                request_changes(
                    &tracks(&before),
                    &tracks(&mapped),
                    &tracks(&after),
                    &tracks(&mapped)
                ),
                expected
            );
        }
    }
}
