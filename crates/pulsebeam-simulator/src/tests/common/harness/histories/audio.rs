use super::*;

pub(super) async fn request(
    state: &mut Transition,
    handles: &PlanHandles,
    pinned: &[u8],
    automatic: bool,
    fixed_ms: Option<u16>,
    witness: &mut Witness,
) -> Result<()> {
    let targets = pinned
        .iter()
        .map(|index| {
            let name = if *index == 0 {
                "anchor"
            } else {
                CAST[usize::from(*index - 1)]
            };
            handles[name]
                .participant_id()
                .context("audio preference has no admitted publisher")
        })
        .collect::<Result<Vec<_>>>()?;
    let issued = Arc::new(AtomicBool::new(false));
    handles["viewer"]
        .shared
        .pending_ops
        .lock()
        .unwrap()
        .push(PendingDriverOp::SetAudioRequest(
            targets,
            automatic,
            fixed_ms,
            issued.clone(),
        ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !issued.load(Ordering::Acquire) {
            state.wait(handles, Duration::from_millis(10)).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("audio desired state was not issued")??;
    state.recovery_at = state.recovery_at.max(Instant::now());
    witness.record(format!(
        "audio-input pinned={pinned:?} automatic={automatic} fixed_ms={fixed_ms:?}"
    ));
    Ok(())
}

pub(super) async fn progress(
    state: &mut Transition,
    handles: &PlanHandles,
    witness: &mut Witness,
) -> Result<()> {
    let handle = &handles["viewer"];
    // Earlier automatic assignments can leave same-generation receiver history.
    // Select only receivers with actual PCM after this progress operation starts.
    handle
        .shared
        .audio_rx
        .lock()
        .unwrap()
        .native_pcm
        .begin_window();
    let mut baseline: Option<(u64, Vec<u8>)> = None;
    let mut last = String::new();
    let remaining =
        (state.recovery_at + Duration::from_secs(30)).saturating_duration_since(Instant::now());
    // Controller-stalled intent is related recovery work, not an ordinary
    // initial-visibility request. Use the existing after-repair deadline.
    let recovering = state.stall.is_some();
    let budget = if recovering {
        remaining
    } else {
        Duration::from_secs(5).min(remaining)
    };
    witness.record(format!(
        "audio-progress-window recovering={recovering} budget_ms={}",
        budget.as_millis()
    ));
    let waiting = tokio::time::timeout(budget, async {
        loop {
            state.clients_safe(handles)?;
            let check = (|| -> Result<()> {
                let observed =
                    observation(handle).context("audio viewer observation unavailable")?;
                let capacity = crate::tests::histories::AUDIO_SLOTS;
                let pinned = observed
                    .submitted
                    .audio
                    .pinned
                    .iter()
                    .take(capacity)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let mapped = observed
                    .snapshot
                    .audio_mapping
                    .values()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                ensure!(
                    mapped.len() <= capacity && pinned.is_subset(&mapped),
                    "audio preference/capacity not applied"
                );
                if !observed.submitted.audio.automatic {
                    ensure!(
                        mapped == pinned,
                        "manual audio acquired an unrequested publication"
                    );
                }
                ensure!(
                    !mapped.is_empty(),
                    "audio progress cannot use an empty assignment"
                );
                let generation = audio_generation(handle)?;
                if baseline
                    .as_ref()
                    .is_some_and(|(before, _)| *before != generation)
                {
                    // Supported recovery can replace the native graph. Fence the
                    // retired window, without restarting the outer recovery deadline.
                    baseline = None;
                    handle
                        .shared
                        .audio_rx
                        .lock()
                        .unwrap()
                        .native_pcm
                        .begin_window();
                    anyhow::bail!("audio receiver generation retired; awaiting a fresh baseline");
                }
                let required = pinned.len().max(1);
                // Signal and intentional silence are both valid. PCM must be fresh
                // and continuous; decoder counters separately reject concealment.
                let tracks = handle.audio_rx().native_pcm.require_progress(
                    generation,
                    2,
                    required,
                    Instant::now(),
                )?;
                let statistics = handle.shared.statistics.lock().unwrap();
                let stats = statistics
                    .as_ref()
                    .context("audio window statistics unavailable")?;
                ensure!(
                    stats.generation == generation,
                    "audio statistics generation mismatch"
                );
                let before = if let Some((before_generation, before)) = &baseline {
                    ensure!(
                        *before_generation == generation,
                        "audio generation changed during measurement"
                    );
                    Some(before.as_slice())
                } else {
                    None
                };
                // Capacity is not an obligation to keep unused native slots decoding.
                // Every credited MID must join fresh PCM to its own decoder progress.
                super::super::super::native_audio::require_decoder_receivers(
                    before,
                    &stats.report_json,
                    &tracks,
                    required,
                )?;
                if baseline.is_none() {
                    baseline = Some((generation, stats.report_json.clone()));
                    handle
                        .shared
                        .audio_rx
                        .lock()
                        .unwrap()
                        .native_pcm
                        .begin_window();
                    anyhow::bail!("audio window opened");
                }
                Ok(())
            })();
            match check {
                Ok(()) => return Ok::<_, anyhow::Error>(()),
                Err(error) => last = format!("{error:#}"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    waiting.with_context(|| format!("audio progress deadline: {last}"))??;
    witness.record(format!(
        "audio-progress-complete generation={}",
        audio_generation(handle)?
    ));
    let observed = observation(handle).context("audio completion observation")?;
    witness.reach(if observed.submitted.audio.automatic {
        "audio-automatic-fresh-decode"
    } else {
        "audio-manual-fresh-decode"
    });
    Ok(())
}

pub(super) async fn pressure(
    state: &mut Transition,
    handles: &PlanHandles,
    witness: &mut Witness,
) -> Result<()> {
    // Four actual desired-state commands, not a loop waiting for convergence.
    for (pinned, automatic, delay) in [
        (vec![0], false, None),
        (vec![], false, None),
        (vec![], true, None),
        (vec![0], false, None),
    ] {
        request(state, handles, &pinned, automatic, delay, witness).await?;
    }
    witness.reach("bounded-signaling-input-burst-issued");
    let settled = Arc::new(Mutex::new(None));
    handles["anchor"]
        .shared
        .pending_ops
        .lock()
        .unwrap()
        .push(PendingDriverOp::ProducerPressure(settled.clone()));
    tokio::time::timeout(Duration::from_secs(2), async {
        while settled.lock().unwrap().is_none() {
            state.wait(handles, Duration::from_millis(10)).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("public producer submissions did not settle")??;
    let (accepted, refused) = settled.lock().unwrap().expect("settled producer input");
    ensure!(accepted + refused == 8, "producer submissions disappeared");
    witness.record(format!("producer-settlement submitted=8 accepted={accepted} backpressure={refused}; admission-is-not-delivery"));
    witness.reach("bounded-public-producer-settlement");
    if !state.holds.is_empty()
        || state.stall.is_some_and(|until| Instant::now() < until)
        || (state.failed && !handles["retry"].connected())
    {
        witness.reach("cross-lane-pressure-before-repair");
    }
    Ok(())
}
