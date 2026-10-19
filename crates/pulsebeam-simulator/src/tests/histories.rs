//! Concrete, bounded programs. Replay never consults the generator.
use super::common::{harness::*, sim_seed};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const CAST: [&str; 3] = ["a", "b", "c"];
pub const CASE: &str = "tests::histories::required_intent_histories";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    Exploration,
    Replacement,
    Generation,
    Stall,
    Materialization,
}
impl Claim {
    fn name(self) -> &'static str {
        match self {
            Self::Exploration => "exploration",
            Self::Replacement => "replacement",
            Self::Generation => "generation",
            Self::Stall => "stall",
            Self::Materialization => "materialization",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Join(u8),
    Leave(u8),
    Crash(u8),
    Readmit(u8),
    Receive(Vec<u8>), // 0 is anchor, 1..=3 are symbolic publication incarnations.
    Repeat,
    Hold(u8),
    Release(u8),
    Pause(u16),
    Stall,
    FailAdmission,
    Retry,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct History {
    pub claim: Claim,
    pub ops: Vec<Op>,
}
#[derive(Clone, Debug, Default)]
pub struct Witness {
    pub applied: usize,
    pub reached: BTreeSet<String>,
    pub events: Vec<String>,
    pub phase: String,
}
impl Witness {
    pub fn record(&mut self, event: String) {
        println!("[history-witness] {event}");
        self.events.push(event);
    }
    pub fn reach(&mut self, claim: &str) {
        self.reached.insert(claim.into());
        self.record(format!("claim={claim} applied={}", self.applied));
    }
}

impl History {
    pub fn validate(&self) -> Result<(), String> {
        if self.ops.is_empty() || self.ops.len() > 32 {
            return Err("history length must be 1..=32".into());
        }
        let mut live = [false; 3];
        let mut admitted = [false; 3];
        let mut held = BTreeSet::new();
        let mut failed = false;
        let mut retried = false;
        let mut stalls = 0;
        let mut receives = 0;
        let mut lifecycle = 0;
        for op in &self.ops {
            match op {
                Op::Join(i) | Op::Readmit(i) => {
                    let i = usize::from(*i);
                    if i >= 3 || live[i] || (matches!(op, Op::Readmit(_)) != admitted[i]) {
                        return Err("invalid admission".into());
                    }
                    live[i] = true;
                    admitted[i] = true;
                    lifecycle += 1;
                }
                Op::Leave(i) | Op::Crash(i) => {
                    let i = usize::from(*i);
                    if i >= 3 || !live[i] {
                        return Err("invalid retirement".into());
                    }
                    live[i] = false;
                    lifecycle += 1;
                }
                Op::Receive(targets) => {
                    let mut seen = BTreeSet::new();
                    for i in targets {
                        if *i > 3 || (*i > 0 && !live[usize::from(*i - 1)]) || !seen.insert(*i) {
                            return Err("invalid complete receive intent".into());
                        }
                    }
                    receives += 1;
                }
                Op::Repeat if receives == 0 => return Err("repeat without issued intent".into()),
                Op::Hold(i) if *i <= 3 && held.insert(*i) => {}
                Op::Release(i) if held.remove(i) => {}
                Op::Hold(_) | Op::Release(_) => return Err("unbalanced link hold".into()),
                Op::Pause(ms) if *ms == 0 || *ms > 500 => {
                    return Err("pause must be 1..=500 ms".into());
                }
                Op::Stall => {
                    stalls += 1;
                    if stalls > 1 {
                        return Err("multiple stalls".into());
                    }
                }
                Op::FailAdmission => {
                    if failed {
                        return Err("multiple materialization failures".into());
                    }
                    failed = true;
                }
                Op::Retry => {
                    if !failed || retried {
                        return Err("retry without unique failed admission".into());
                    }
                    retried = true;
                }
                _ => {}
            }
        }
        if !held.is_empty() || (failed && !retried) {
            return Err("unrepaired fault".into());
        }
        let meaningful = match self.claim {
            Claim::Exploration => lifecycle > 0,
            Claim::Replacement => receives >= 3 && self.ops.contains(&Op::Hold(0)),
            Claim::Generation => {
                self.ops.contains(&Op::Crash(0))
                    && self.ops.contains(&Op::Readmit(0))
                    && self.ops.contains(&Op::Hold(1))
            }
            Claim::Stall => stalls == 1 && receives > 0,
            Claim::Materialization => failed && retried,
        };
        if !meaningful {
            return Err("defining interaction removed".into());
        }
        Ok(())
    }
    pub fn encode(&self, seed: u64, witness: &Witness) -> String {
        let mut out = format!(
            "pulsebeam-history-v1\nseed {seed}\ncase {CASE}\nclaim {}\n",
            self.claim.name()
        );
        for op in &self.ops {
            let line = match op {
                Op::Join(i) => format!("join {i}"),
                Op::Leave(i) => format!("leave {i}"),
                Op::Crash(i) => format!("crash {i}"),
                Op::Readmit(i) => format!("readmit {i}"),
                Op::Receive(ids) => format!(
                    "receive{}",
                    ids.iter().map(|i| format!(" {i}")).collect::<String>()
                ),
                Op::Repeat => "repeat".into(),
                Op::Hold(i) => format!("hold {i}"),
                Op::Release(i) => format!("release {i}"),
                Op::Pause(ms) => format!("pause {ms}"),
                Op::Stall => "stall".into(),
                Op::FailAdmission => "fail-admission".into(),
                Op::Retry => "retry".into(),
            };
            out.push_str(&format!("op {line}\n"));
        }
        out.push_str(&format!(
            "witness phase={} applied={} reached={:?}\n",
            witness.phase, witness.applied, witness.reached
        ));
        for event in &witness.events {
            out.push_str(&format!("witness {event}\n"));
        }
        out
    }
    pub fn decode(input: &str) -> Result<(u64, Self), String> {
        let mut lines = input.lines();
        if lines.next() != Some("pulsebeam-history-v1") {
            return Err("unsupported history version".into());
        }
        let seed = lines
            .next()
            .and_then(|s| s.strip_prefix("seed "))
            .ok_or("missing seed")?
            .parse()
            .map_err(|_| "invalid seed")?;
        if lines.next() != Some(&format!("case {CASE}")) {
            return Err("unsupported replay selection".into());
        }
        let claim = match lines.next() {
            Some("claim exploration") => Claim::Exploration,
            Some("claim replacement") => Claim::Replacement,
            Some("claim generation") => Claim::Generation,
            Some("claim stall") => Claim::Stall,
            Some("claim materialization") => Claim::Materialization,
            _ => return Err("unsupported claim".into()),
        };
        let mut ops = Vec::new();
        for line in lines {
            if line.starts_with("witness ") {
                continue;
            }
            let words = line.split_whitespace().collect::<Vec<_>>();
            let byte = |s: &str| {
                s.parse::<u8>()
                    .map_err(|_| "invalid participant".to_string())
            };
            let op = match words.as_slice() {
                ["op", "join", i] => Op::Join(byte(i)?),
                ["op", "leave", i] => Op::Leave(byte(i)?),
                ["op", "crash", i] => Op::Crash(byte(i)?),
                ["op", "readmit", i] => Op::Readmit(byte(i)?),
                ["op", "receive", ids @ ..] => {
                    Op::Receive(ids.iter().map(|s| byte(s)).collect::<Result<_, _>>()?)
                }
                ["op", "repeat"] => Op::Repeat,
                ["op", "hold", i] => Op::Hold(byte(i)?),
                ["op", "release", i] => Op::Release(byte(i)?),
                ["op", "pause", ms] => Op::Pause(ms.parse().map_err(|_| "invalid pause")?),
                ["op", "stall"] => Op::Stall,
                ["op", "fail-admission"] => Op::FailAdmission,
                ["op", "retry"] => Op::Retry,
                _ => return Err(format!("unsupported instruction: {line}")),
            };
            ops.push(op);
        }
        let history = Self { claim, ops };
        history.validate()?;
        Ok((seed, history))
    }
}

fn forced() -> Vec<History> {
    use Op::*;
    vec![
        History {
            claim: Claim::Replacement,
            ops: vec![
                Join(0),
                Hold(0),
                Receive(vec![1]),
                Receive(vec![0]),
                Receive(vec![]),
                Repeat,
                Release(0),
                Receive(vec![1]),
            ],
        },
        History {
            claim: Claim::Generation,
            ops: vec![
                Join(0),
                Receive(vec![1]),
                Hold(1),
                Pause(100),
                Crash(0),
                Readmit(0),
                Release(1),
                Receive(vec![1]),
            ],
        },
        History {
            claim: Claim::Stall,
            ops: vec![Stall, Receive(vec![]), Pause(500), Receive(vec![0])],
        },
        History {
            claim: Claim::Materialization,
            ops: vec![FailAdmission, Retry],
        },
    ]
}

fn strategy() -> impl Strategy<Value = History> {
    (
        0usize..5,
        prop::collection::vec((0u8..3, any::<bool>(), 0u8..5), 2..7),
    )
        .prop_map(|(family, choices)| {
            let mut history = if family == 0 {
                History {
                    claim: Claim::Exploration,
                    ops: Vec::new(),
                }
            } else {
                forced().remove(family - 1)
            };
            let mut live = [false; 3];
            let mut admitted = [false; 3];
            if matches!(history.claim, Claim::Replacement | Claim::Generation) {
                live[0] = true;
                admitted[0] = true;
            }
            let ops = &mut history.ops;
            for (i, crash, intent) in choices {
                let index = usize::from(i);
                if live[index] {
                    ops.push(if crash { Op::Crash(i) } else { Op::Leave(i) });
                    live[index] = false;
                } else {
                    ops.push(if admitted[index] {
                        Op::Readmit(i)
                    } else {
                        Op::Join(i)
                    });
                    live[index] = true;
                    admitted[index] = true;
                }
                if intent > 1 {
                    ops.push(Op::Receive(if intent == 2 {
                        vec![]
                    } else {
                        std::iter::once(0)
                            .chain((0u8..3).filter(|i| live[usize::from(*i)]).map(|i| i + 1))
                            .collect()
                    }));
                }
                if intent == 4 {
                    ops.push(Op::Repeat);
                }
            }
            history
        })
}

fn output_dir() -> PathBuf {
    let path = std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("pulsebeam-histories-{}", std::process::id()))
        });
    std::fs::create_dir_all(&path).expect("history output directory");
    path
}

fn execute(history: &History) -> Result<Witness, (String, Witness)> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static EXECUTION: AtomicUsize = AtomicUsize::new(0);
    let path = output_dir().join(format!(
        "program-{}-{}.history",
        std::process::id(),
        EXECUTION.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, history.encode(sim_seed(), &Witness::default())).unwrap();
    let result_path = path.with_extension("result");
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::histories::program_probe",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PULSEBEAM_SIM_HISTORY", &path)
        .env("PULSEBEAM_HISTORY_RESULT", &result_path)
        .env("PULSEBEAM_SIM_SEED", sim_seed().to_string())
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()),
        )
        .output()
        .expect("isolated history process");
    let mut witness = Witness::default();
    if let Ok(saved) = std::fs::read_to_string(&result_path) {
        for line in saved.lines() {
            if let Some(s) = line.strip_prefix("phase ") {
                witness.phase = s.into();
            } else if let Some(s) = line.strip_prefix("applied ") {
                witness.applied = s.parse().expect("applied witness count");
            } else if let Some(s) = line.strip_prefix("reach ") {
                witness.reached.insert(s.into());
            } else if let Some(s) = line.strip_prefix("event ") {
                witness.events.push(s.into());
            }
        }
    }
    if result.status.success() {
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(result_path).unwrap();
        println!(
            "[history-execution] claim={} applied={} reached={:?}",
            history.claim.name(),
            witness.applied,
            witness.reached
        );
        Ok(witness)
    } else {
        let trace = path.with_extension("trace");
        let mut log = result.stdout;
        log.extend_from_slice(&result.stderr);
        std::fs::write(&trace, log).unwrap();
        let message = std::fs::read_to_string(path.with_extension("failure"))
            .unwrap_or_else(|_| format!("child exited without result: {}", result.status));
        eprintln!("[history-failure] trace={}", trace.display());
        Err((message, witness))
    }
}

#[test]
#[ignore = "isolated concrete-history subprocess fixture"]
fn program_probe() {
    let path =
        PathBuf::from(std::env::var_os("PULSEBEAM_SIM_HISTORY").expect("concrete program input"));
    let result_path =
        PathBuf::from(std::env::var_os("PULSEBEAM_HISTORY_RESULT").expect("result output"));
    let (seed, history) = History::decode(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(seed, sim_seed());
    let result = execute_local(&history);
    let witness = match &result {
        Ok(witness) | Err((_, witness)) => witness,
    };
    let mut saved = format!("phase {}\napplied {}\n", witness.phase, witness.applied);
    for claim in &witness.reached {
        saved.push_str(&format!("reach {claim}\n"));
    }
    for event in &witness.events {
        saved.push_str(&format!("event {event}\n"));
    }
    std::fs::write(result_path, saved).unwrap();
    if let Err((error, _)) = result {
        std::fs::write(path.with_extension("failure"), &error).unwrap();
        panic!("{error}");
    }
}

fn execute_local(history: &History) -> Result<Witness, (String, Witness)> {
    history.validate().expect("valid history before execution");
    let witness = Arc::new(Mutex::new(Witness::default()));
    let shared = witness.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut room = Room::new("history")
            .with_participant(Participant::single_publisher("anchor"))
            .with_participant(Participant::manual_subscriber("viewer", 4))
            .with_participant(Participant::manual_subscriber("continuing", 1))
            .with_participant(Participant::manual_subscriber("retry", 1).starts_disconnected());
        for who in CAST {
            room = room.with_participant(Participant::single_publisher(who).starts_disconnected());
        }
        LocalNodeSim::new()
            .with_subnet(71)
            .with_room(room)
            .with_room(
                Room::new("bystander")
                    .with_participant(Participant::single_publisher("foreign"))
                    .with_participant(Participant::subscriber("bystander")),
            )
            .run(vec![
                Step::WaitForPublication {
                    description: "admit history world",
                    publisher: "anchor",
                    timeout: Duration::from_secs(30),
                },
                Step::IntentHistory {
                    history: history.clone(),
                    witness: shared,
                },
            ]);
    }));
    let witness = witness.lock().unwrap().clone();
    match result {
        Ok(()) => Ok(witness),
        Err(error) => {
            let message = error
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| error.downcast_ref::<&str>().map(|s| (*s).into()))
                .unwrap_or_else(|| "non-string panic".into());
            Err((message, witness))
        }
    }
}

fn same_failure(original: &(String, Witness), candidate: &(String, Witness)) -> bool {
    original.1.phase == candidate.1.phase
        && original.1.reached == candidate.1.reached
        && original.0.lines().find(|s| !s.is_empty()) == candidate.0.lines().find(|s| !s.is_empty())
}

fn reduce(
    history: &History,
    failure: &(String, Witness),
    executions: &mut usize,
    evaluate: impl Fn(&History) -> Result<Witness, (String, Witness)>,
) -> (History, (String, Witness)) {
    let mut best = history.clone();
    let mut best_failure = failure.clone();
    for index in (0..history.ops.len()).rev().take(8) {
        if index >= best.ops.len() {
            continue;
        }
        let mut candidate = best.clone();
        candidate.ops.remove(index);
        if candidate.validate().is_err() {
            continue;
        }
        *executions += 1;
        if let Err(next) = evaluate(&candidate)
            && same_failure(failure, &next)
        {
            best = candidate;
            best_failure = next;
        }
    }
    (best, best_failure)
}

fn run_required(history: History, label: &str, reductions: &mut usize) -> Witness {
    let seed = sim_seed();
    let dir = output_dir();
    let original_path = dir.join(format!("{label}.original.history"));
    // Retain the exact input even if the process terminates before producing a witness.
    std::fs::write(&original_path, history.encode(seed, &Witness::default())).unwrap();
    match execute(&history) {
        Ok(witness) => {
            std::fs::remove_file(original_path).unwrap();
            witness
        }
        Err(failure) => {
            std::fs::write(&original_path, history.encode(seed, &failure.1)).unwrap();
            let (best, best_failure) = reduce(&history, &failure, reductions, execute);
            let reduced = dir.join(format!("{label}.reduced.history"));
            std::fs::write(&reduced, best.encode(seed, &best_failure.1)).unwrap();
            panic!(
                "history failed: {}\noriginal={}\nreduced={}\nseed={seed} claim={} reached={:?} reduction_executions={}\nreplay: PULSEBEAM_SIM_HISTORY={} ./bazel run //:replay -- --seed {seed} --exact --filter {CASE}",
                failure.0,
                original_path.display(),
                reduced.display(),
                history.claim.name(),
                failure.1.reached,
                reductions,
                reduced.display()
            );
        }
    }
}

#[test]
fn required_intent_histories() {
    if let Some(path) = std::env::var_os("PULSEBEAM_SIM_HISTORY") {
        let (seed, history) =
            History::decode(&std::fs::read_to_string(Path::new(&path)).expect("read history"))
                .expect("supported concrete history");
        assert_eq!(
            seed,
            sim_seed(),
            "whole-run seed differs from concrete artifact"
        );
        match execute_local(&history) {
            Err((error, witness)) => {
                panic!("direct history replay failed: {error}; witnesses={witness:?}")
            }
            Ok(witness) => println!(
                "[history-replay] seed={seed} claim={} applied={} reached={:?}",
                history.claim.name(),
                witness.applied,
                witness.reached
            ),
        }
        return;
    }
    let mut reductions = 0;
    let mut applied = 0;
    let mut claims = BTreeMap::<String, usize>::new();
    let mut run = |history, label: &str| {
        let witness = run_required(history, label, &mut reductions);
        applied += witness.applied;
        for claim in witness.reached {
            *claims.entry(claim).or_default() += 1;
        }
    };
    for (index, history) in forced().into_iter().enumerate() {
        run(history, &format!("forced-{index}"));
    }
    let mut rng_seed = [0u8; 32];
    rng_seed[..8].copy_from_slice(&sim_seed().to_le_bytes());
    let config = || Config {
        failure_persistence: None,
        ..Config::default()
    };
    let mut runner = TestRunner::new_with_rng(
        config(),
        TestRng::from_seed(RngAlgorithm::ChaCha, &rng_seed),
    );
    // The committed corpus remains useful input diversity. Its comments are not concrete programs.
    let mut retained = 0;
    for line in include_str!("properties.regressions")
        .lines()
        .filter(|s| s.starts_with("cc "))
    {
        let hex = line.split_whitespace().nth(1).unwrap();
        let bytes = (0..32)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect::<Vec<_>>();
        let mut regression =
            TestRunner::new_with_rng(config(), TestRng::from_seed(RngAlgorithm::ChaCha, &bytes));
        let history = strategy().new_tree(&mut regression).unwrap().current();
        run(history, &format!("retained-{retained}"));
        retained += 1;
    }
    for index in 0..24 {
        let history = strategy().new_tree(&mut runner).unwrap().current();
        run(history, &format!("novel-{index}"));
    }
    println!(
        "[simulation-generated] family=intent-histories seed={} budget=24 novel=24 retained={retained} forced=4 reductions={reductions} applied={applied} claims={claims:?}",
        sim_seed()
    );
}

#[test]
fn concrete_replay_uses_saved_program_in_a_fresh_process() {
    let history = forced().remove(0);
    let seed = sim_seed();
    let path = output_dir().join("replay-proof.history");
    std::fs::write(&path, history.encode(seed, &Witness::default())).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CASE, "--nocapture", "--test-threads=1"])
        .env("PULSEBEAM_SIM_HISTORY", &path)
        .env("PULSEBEAM_SIM_SEED", seed.to_string())
        .env("RUST_LOG", "off")
        .output()
        .unwrap();
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(
        result.status.success(),
        "{output}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.contains("[history-replay]") && output.contains("outstanding-withdrawal"));
    assert!(
        !output.contains("[simulation-generated] family=intent-histories"),
        "direct replay regenerated cases"
    );
    assert!(output.contains(&format!("applied={}", history.ops.len())));
}

#[test]
fn reduction_retains_the_reached_claim_not_a_different_harness_error() {
    let history = History {
        claim: Claim::Exploration,
        ops: vec![
            Op::Join(0),
            Op::Receive(vec![1]),
            Op::Repeat,
            Op::Pause(100),
            Op::Leave(0),
        ],
    };
    let witness = Witness {
        phase: "oracle".into(),
        reached: BTreeSet::from(["retirement-in-flight".into()]),
        ..Witness::default()
    };
    let evaluate = |candidate: &History| {
        candidate.validate().unwrap();
        if !candidate.ops.contains(&Op::Leave(0)) {
            return Ok(witness.clone());
        }
        if !candidate.ops.contains(&Op::Pause(100)) {
            return Err(("harness error".into(), witness.clone()));
        }
        Err(("wrong origin".into(), witness.clone()))
    };
    let failure = evaluate(&history).unwrap_err();
    let mut executions = 0;
    let (best, reduced) = reduce(&history, &failure, &mut executions, evaluate);
    assert!(best.ops.len() < history.ops.len() && executions > 0);
    assert!(best.ops.contains(&Op::Pause(100)) && best.ops.contains(&Op::Leave(0)));
    assert!(same_failure(&failure, &reduced));
    let mut missed = reduced;
    missed.1.reached.clear();
    assert!(!same_failure(&failure, &missed));
}

#[test]
fn concrete_format_and_reduction_preserve_validity() {
    for history in forced() {
        history.validate().unwrap();
        assert_eq!(
            History::decode(&history.encode(17, &Witness::default())).unwrap(),
            (17, history.clone())
        );
        for i in 0..history.ops.len() {
            let mut reduced = history.clone();
            reduced.ops.remove(i);
            if reduced.validate().is_ok() {
                assert!(!reduced.ops.is_empty());
            }
        }
    }
    assert!(History::decode("pulsebeam-history-v2\n").is_err());
    assert!(
        History {
            claim: Claim::Exploration,
            ops: vec![Op::Leave(0)]
        }
        .validate()
        .is_err()
    );
    let mut runner = TestRunner::deterministic();
    for _ in 0..100 {
        strategy()
            .new_tree(&mut runner)
            .unwrap()
            .current()
            .validate()
            .unwrap();
    }
}
