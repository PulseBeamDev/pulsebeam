use super::*;

pub(super) const CASES: usize = 24;
pub(super) const PROPOSALS: usize = 4;
pub(super) const MAIN_OPERATIONS: usize = CASES * MAX_OPS;
pub(super) const TOTAL_OPERATIONS: usize = MAIN_OPERATIONS + MAX_REDUCTIONS * MAX_OPS;

fn features(history: &History, applied: usize) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut held = BTreeSet::new();
    let mut stalled = false;
    let mut failed = false;
    let mut previous = "start".to_string();
    for op in history.ops.iter().take(applied) {
        let phase = if !held.is_empty() {
            "held-input"
        } else if failed {
            "unrepaired-admission-input"
        } else if stalled {
            "after-stall-input"
        } else {
            "normal"
        };
        let kind = match op {
            Op::Join(i) => format!("join:{i}"),
            Op::Readmit(i) => format!("readmit:{i}"),
            Op::Leave(i) => format!("leave:{i}"),
            Op::Crash(i) => format!("crash:{i}"),
            Op::Receive(targets) => format!("video:{targets:?}"),
            Op::Audio {
                pinned,
                automatic,
                fixed_ms,
            } => format!("audio:{pinned:?}:{automatic}:{fixed_ms:?}"),
            Op::DeclareData(mode) => format!("declare:{}", mode.name()),
            Op::ReplaceData(mode) => format!("replace:{}", mode.name()),
            Op::SendData(mode, _) => format!("send:{}", mode.name()),
            Op::Hold(i) => format!("hold:{i}"),
            Op::Release(i) => format!("release:{i}"),
            Op::ReplaceSource(i) => format!("source:{i}"),
            Op::ReleaseSource(i) => format!("old-source:{i}"),
            Op::Pause(_) => "pause".into(),
            Op::Repeat => "repeat".into(),
            Op::AudioProgress => "audio-progress".into(),
            Op::Pressure => "pressure".into(),
            Op::Stall => "stall".into(),
            Op::FailAdmission => "fail-admission".into(),
            Op::Retry => "retry".into(),
        };
        out.insert(format!("operation:{phase}:{kind}"));
        out.insert(format!("transition:{previous}->{kind}"));
        previous = kind;
        match op {
            Op::Hold(i) => {
                held.insert(*i);
            }
            Op::Release(i) => {
                held.remove(i);
            }
            Op::Stall => stalled = true,
            Op::FailAdmission => failed = true,
            Op::Retry => failed = false,
            _ => {}
        }
    }
    out
}

fn mutate(history: &History, choice: usize) -> Option<History> {
    let mut candidate = history.clone();
    match choice % 3 {
        0 if candidate.ops.len() < MAX_OPS => {
            let at = candidate
                .ops
                .iter()
                .position(|op| matches!(op, Op::Receive(_)))?
                + 1;
            candidate.ops.insert(at, Op::Repeat);
        }
        1 if candidate.ops.len() < MAX_OPS => {
            let at = candidate
                .ops
                .iter()
                .position(|op| matches!(op, Op::Hold(_) | Op::Stall))
                .map_or(0, |at| at + 1);
            candidate.ops.insert(at, Op::Pause(100));
        }
        _ => {
            let Op::Audio { automatic, .. } = candidate
                .ops
                .iter_mut()
                .find(|op| matches!(op, Op::Audio { .. }))?
            else {
                unreachable!()
            };
            *automatic = !*automatic;
        }
    }
    (candidate != *history && candidate.validate().is_ok()).then_some(candidate)
}

fn select(pool: &[History], seen: &BTreeSet<String>, guided: bool) -> usize {
    let mut chosen = 0;
    let mut best = 0;
    if guided {
        for (index, candidate) in pool.iter().enumerate() {
            let score = features(candidate, candidate.ops.len())
                .difference(seen)
                .count();
            if score > best {
                best = score;
                chosen = index;
            }
        }
    }
    chosen
}

fn arm(mode: &str) -> BTreeSet<String> {
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(&sim_seed().to_le_bytes());
    let mut runner = TestRunner::new_with_rng(
        Config {
            failure_persistence: None,
            ..Config::default()
        },
        TestRng::from_seed(RngAlgorithm::ChaCha, &seed),
    );
    let mut seen = BTreeSet::new();
    let mut reached = BTreeSet::new();
    let mut previous = None;
    let mut operations = 0;
    let mut reductions = 0;
    for index in 0..CASES {
        let mut pool = Vec::new();
        for proposal in 0..PROPOSALS {
            // Both arms consume the same bounded generation budget. Mutation
            // uses only concrete inputs, never private state or failing rerolls.
            let generated = strategy()
                .new_tree(&mut runner)
                .unwrap()
                .current()
                .with_audio_lane();
            let candidate = if proposal == 0 {
                generated
            } else {
                previous
                    .as_ref()
                    .and_then(|history| mutate(history, index + proposal))
                    .unwrap_or(generated)
            };
            candidate.validate().expect("validated search proposal");
            pool.push(candidate);
        }
        let chosen = select(&pool, &seen, mode == "guided");
        let history = pool.swap_remove(chosen);
        operations += history.ops.len();
        assert!(operations <= MAIN_OPERATIONS);
        let witness = run_required(
            history.clone(),
            &format!("search-{mode}-{index}"),
            &mut reductions,
        );
        assert_eq!(witness.applied, history.ops.len());
        seen.extend(features(&history, witness.applied));
        reached.extend(witness.reached);
        previous = Some(history);
    }
    println!(
        "[history-search] mode={mode} seed={} evaluated={CASES} proposals={} applied={operations} main_operation_budget={MAIN_OPERATIONS} max_history_ops={MAX_OPS} reduction_budget={MAX_REDUCTIONS} total_operation_budget={} reductions={reductions} input_novelty={} public_claims={reached:?}",
        sim_seed(),
        CASES * PROPOSALS,
        TOTAL_OPERATIONS,
        seen.len(),
    );
    seen
}

pub(super) fn run(mode: &str) {
    match mode {
        "guided" | "unguided" => {
            arm(mode);
        }
        "compare" => {
            let unguided = arm("unguided");
            let guided = arm("guided");
            println!(
                "[history-search-comparison] equal_cases={CASES} equal_proposals={} unguided_novelty={} guided_novelty={} guided_unique={} unguided_unique={}; public-input-coverage-not-correctness-or-performance-benefit",
                CASES * PROPOSALS,
                unguided.len(),
                guided.len(),
                guided.difference(&unguided).count(),
                unguided.difference(&guided).count()
            );
        }
        _ => panic!("unsupported advisory history search mode: {mode}"),
    }
}

#[test]
fn novelty_selection_is_stable_bounded_and_uses_only_executed_inputs() {
    let base = forced().remove(0).with_audio_lane();
    let changed = mutate(&base, 0).unwrap();
    assert_eq!(features(&base, 0), BTreeSet::new());
    assert!(features(&base, 1).len() < features(&base, base.ops.len()).len());
    let pool = vec![base.clone(), changed];
    let seen = features(&base, base.ops.len());
    assert_eq!(select(&pool, &seen, false), 0);
    assert_eq!(select(&pool, &seen, true), 1);
    assert_eq!(
        select(&[base.clone(), base.clone()], &BTreeSet::new(), true),
        0
    );
    for choice in 0..9 {
        if let Some(candidate) = mutate(&base, choice) {
            candidate.validate().unwrap();
            assert!(candidate.ops.len() <= MAX_OPS);
            assert_eq!(
                History::decode(&candidate.encode(17, &Witness::default()))
                    .unwrap()
                    .1,
                candidate
            );
        }
    }
    assert_eq!(MAIN_OPERATIONS, 768);
    assert_eq!(CASES * PROPOSALS, 96);
}
