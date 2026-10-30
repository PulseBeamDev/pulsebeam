//! Concrete, bounded programs. Replay never consults the generator.
use super::common::{harness::*, sim_seed};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod search;
const MAX_OPS: usize = 32;
const MAX_REDUCTIONS: usize = 8;
const HEADER: &str = "pulsebeam-history-v2";

pub const CAST: [&str; 3] = ["a", "b", "c"];
pub const CASE: &str = "tests::histories::required_intent_histories";
pub const AUDIO_SLOTS: usize = 2;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Delivery {
    Latest,
    Lossy,
    Ordered,
}

impl Delivery {
    pub fn name(self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::Lossy => "lossy",
            Self::Ordered => "ordered",
        }
    }

    pub fn mode(self) -> pulsebeam_agent_native::agent_core::TopicMode {
        use pulsebeam_agent_native::agent_core::TopicMode;
        match self {
            Self::Latest => TopicMode::Latest,
            Self::Lossy => TopicMode::Lossy,
            Self::Ordered => TopicMode::Ordered,
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "latest" => Ok(Self::Latest),
            "lossy" => Ok(Self::Lossy),
            "ordered" => Ok(Self::Ordered),
            _ => Err("invalid delivery mode".into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Join(u8),
    Leave(u8),
    Crash(u8),
    Readmit(u8),
    ReplaceSource(u8),
    ReleaseSource(u8),
    DeclareData(Delivery),
    SendData(Delivery, u8),
    ReplaceData(Delivery),
    Audio {
        pinned: Vec<u8>,
        automatic: bool,
        fixed_ms: Option<u16>,
    },
    AudioProgress,
    Pressure,
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
    fn with_initial_source_replacement(mut self) -> Self {
        assert_eq!(self.ops.first(), Some(&Op::Join(0)));
        self.ops.splice(
            1..1,
            [
                Op::Receive(vec![1]),
                Op::ReplaceSource(0),
                Op::ReleaseSource(0),
                Op::Receive(vec![0]),
            ],
        );
        self
    }

    fn with_data_lane(mut self) -> Self {
        if self.ops.len() > MAX_OPS - 4 {
            return self;
        }
        let delivery = match self.claim {
            Claim::Replacement | Claim::Stall => Delivery::Latest,
            Claim::Generation | Claim::Materialization => Delivery::Ordered,
            Claim::Exploration => Delivery::Lossy,
        };
        let position = self
            .ops
            .iter()
            .position(|op| match self.claim {
                Claim::Replacement | Claim::Generation => matches!(op, Op::Release(_)),
                Claim::Stall => matches!(op, Op::Pause(_)),
                Claim::Materialization => matches!(op, Op::Retry),
                Claim::Exploration => false,
            })
            .unwrap_or(self.ops.len() / 2);
        self.ops.splice(
            position..position,
            [Op::ReplaceData(delivery), Op::SendData(delivery, 1)],
        );
        self.ops
            .splice(0..0, [Op::DeclareData(delivery), Op::SendData(delivery, 0)]);
        self
    }

    fn with_audio_lane(mut self) -> Self {
        if self.ops.len() > MAX_OPS - 5 {
            return self;
        }
        let fault = self
            .ops
            .iter()
            .position(|op| matches!(op, Op::Hold(_) | Op::Stall | Op::FailAdmission))
            .unwrap_or(self.ops.len());
        let first_join = self
            .ops
            .iter()
            .position(|op| matches!(op, Op::Join(_)))
            .map_or(0, |position| position + 1);
        let before = if first_join <= fault { first_join } else { 0 };
        self.ops.splice(
            before..before,
            [
                Op::Audio {
                    pinned: vec![0],
                    automatic: false,
                    fixed_ms: None,
                },
                Op::AudioProgress,
            ],
        );
        let pressure = self
            .ops
            .iter()
            .position(|op| matches!(op, Op::Hold(_) | Op::Stall | Op::FailAdmission))
            .map_or(self.ops.len(), |position| position + 1);
        self.ops.insert(pressure, Op::Pressure);
        self.ops.extend([
            Op::Audio {
                pinned: vec![0],
                automatic: true,
                fixed_ms: None,
            },
            Op::AudioProgress,
        ]);
        self
    }

    pub fn uses_audio(&self) -> bool {
        self.ops.iter().any(|op| matches!(op, Op::Audio { .. }))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.ops.is_empty() || self.ops.len() > MAX_OPS {
            return Err("history length must be 1..=32".into());
        }
        let mut live = [false; 3];
        let mut admitted = [false; 3];
        let mut replaced_sources = [false; 3];
        let mut requested = BTreeSet::from([0u8]);
        let mut data = BTreeSet::new();
        let mut data_tags = BTreeSet::new();
        let mut audio_requested = false;
        let mut audio_pins = BTreeSet::new();
        let mut automatic_audio = false;
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
                    let index = usize::from(*i);
                    if index >= 3 || !live[index] {
                        return Err("invalid retirement".into());
                    }
                    live[index] = false;
                    replaced_sources[index] = false;
                    requested.remove(&(*i + 1));
                    audio_pins.remove(&(*i + 1));
                    lifecycle += 1;
                }
                Op::ReplaceSource(i) | Op::ReleaseSource(i) => {
                    let index = usize::from(*i);
                    if index >= 3
                        || !live[index]
                        || !requested.contains(&(*i + 1))
                        || held.contains(&(*i + 1))
                        || (matches!(op, Op::ReleaseSource(_)) && !replaced_sources[index])
                    {
                        return Err("invalid source lifetime operation".into());
                    }
                    replaced_sources[index] = true;
                }
                Op::DeclareData(mode) => {
                    if !data.insert(*mode) {
                        return Err("invalid data declaration".into());
                    }
                }
                Op::SendData(mode, _) | Op::ReplaceData(mode) => {
                    if !data.contains(mode) {
                        return Err("data operation without declaration".into());
                    }
                    if let Op::SendData(_, tag) = op
                        && (*tag >= 254 || !data_tags.insert((*mode, *tag)))
                    {
                        return Err("reserved or reused data tag".into());
                    }
                }
                Op::Audio {
                    pinned,
                    automatic,
                    fixed_ms,
                } => {
                    if fixed_ms.is_some() {
                        return Err(
                            "native history profile does not support fixed audio playout".into(),
                        );
                    }
                    let mut seen = BTreeSet::new();
                    for target in pinned {
                        if *target > 3
                            || (*target > 0 && !live[usize::from(*target - 1)])
                            || !seen.insert(*target)
                        {
                            return Err("invalid audio preference".into());
                        }
                    }
                    audio_requested = true;
                    audio_pins = seen;
                    automatic_audio = *automatic;
                }
                Op::AudioProgress => {
                    if !audio_requested || (audio_pins.is_empty() && !automatic_audio) {
                        return Err("audio progress requires a live nonempty audio request".into());
                    }
                }
                Op::Pressure => {
                    if !audio_requested {
                        return Err("pressure requires an issued audio request".into());
                    }
                    audio_pins = BTreeSet::from([0]);
                    automatic_audio = false;
                }
                Op::Receive(targets) => {
                    let mut seen = BTreeSet::new();
                    for i in targets {
                        if *i > 3 || (*i > 0 && !live[usize::from(*i - 1)]) || !seen.insert(*i) {
                            return Err("invalid complete receive intent".into());
                        }
                    }
                    requested = seen;
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
            "{HEADER}\nseed {seed}\ncase {CASE}\nclaim {}\n",
            self.claim.name()
        );
        for op in &self.ops {
            let line = match op {
                Op::Join(i) => format!("join {i}"),
                Op::Leave(i) => format!("leave {i}"),
                Op::Crash(i) => format!("crash {i}"),
                Op::Readmit(i) => format!("readmit {i}"),
                Op::ReplaceSource(i) => format!("replace-source {i}"),
                Op::ReleaseSource(i) => format!("release-source {i}"),
                Op::DeclareData(mode) => format!("declare-data {}", mode.name()),
                Op::SendData(mode, tag) => format!("send-data {} {tag}", mode.name()),
                Op::ReplaceData(mode) => format!("replace-data {}", mode.name()),
                Op::Audio {
                    pinned,
                    automatic,
                    fixed_ms,
                } => format!(
                    "audio {} {}{}",
                    if *automatic { "auto" } else { "manual" },
                    fixed_ms.map_or_else(|| "adaptive".into(), |delay| delay.to_string()),
                    pinned
                        .iter()
                        .map(|target| format!(" {target}"))
                        .collect::<String>()
                ),
                Op::AudioProgress => "audio-progress".into(),
                Op::Pressure => "pressure".into(),
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
        if lines.next() != Some(HEADER) {
            return Err("unsupported history format".into());
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
                ["op", "replace-source", i] => Op::ReplaceSource(byte(i)?),
                ["op", "release-source", i] => Op::ReleaseSource(byte(i)?),
                ["op", "declare-data", mode] => Op::DeclareData(Delivery::parse(mode)?),
                ["op", "send-data", mode, tag] => Op::SendData(Delivery::parse(mode)?, byte(tag)?),
                ["op", "replace-data", mode] => Op::ReplaceData(Delivery::parse(mode)?),
                ["op", "audio", automatic, delay, pinned @ ..] => Op::Audio {
                    automatic: match *automatic {
                        "auto" => true,
                        "manual" => false,
                        _ => return Err("invalid audio fill policy".into()),
                    },
                    fixed_ms: if *delay == "adaptive" {
                        None
                    } else {
                        Some(delay.parse().map_err(|_| "invalid audio delay")?)
                    },
                    pinned: pinned
                        .iter()
                        .map(|target| byte(target))
                        .collect::<Result<_, _>>()?,
                },
                ["op", "audio-progress"] => Op::AudioProgress,
                ["op", "pressure"] => Op::Pressure,
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
            let history = if matches!(history.claim, Claim::Replacement | Claim::Generation) {
                history.with_initial_source_replacement()
            } else {
                history
            };
            history.with_data_lane()
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
        let audio = history.uses_audio();
        let anchor = Participant::single_publisher("anchor");
        let viewer = Participant::manual_subscriber("viewer", 4);
        let mut room = Room::new("history")
            .with_participant(if audio {
                anchor.speaking_at(-20).with_native_pcm_audio()
            } else {
                anchor
            })
            .with_participant(if audio {
                viewer.hearing(AUDIO_SLOTS)
            } else {
                viewer
            })
            .with_participant(Participant::manual_subscriber("continuing", 1))
            .with_participant(Participant::manual_subscriber("retry", 1).starts_disconnected());
        for who in CAST {
            let publisher = Participant::single_publisher(who).starts_disconnected();
            room = room.with_participant(if audio {
                publisher.speaking_at(-30)
            } else {
                publisher
            });
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
    for index in (0..history.ops.len()).rev().take(MAX_REDUCTIONS) {
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
    if let Ok(mode) = std::env::var("PULSEBEAM_HISTORY_SEARCH")
        && mode != "off"
    {
        search::run(&mode);
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
        run(history.with_audio_lane(), &format!("forced-{index}"));
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
    for index in 0..search::CASES {
        let history = strategy()
            .new_tree(&mut runner)
            .unwrap()
            .current()
            .with_audio_lane();
        run(history, &format!("novel-{index}"));
    }
    println!(
        "[simulation-generated] family=intent-histories seed={} budget=24 novel=24 retained={retained} forced=4 reductions={reductions} applied={applied} claims={claims:?}",
        sim_seed()
    );
}

#[test]
fn concrete_replay_uses_saved_program_in_a_fresh_process() {
    let composed = forced()
        .remove(0)
        .with_initial_source_replacement()
        .with_data_lane()
        .with_audio_lane();
    // This is the exact previously failing concrete program, not an RNG state
    // interpreted by a subsequently expanded strategy.
    let saved = History::decode(include_str!("histories/admission-replacement.history"))
        .unwrap()
        .1;
    let cases = [
        (
            "composed",
            composed,
            vec![
                "outstanding-withdrawal",
                "fresh-replacement-source-after-retired-close",
                "topic-latest-fresh-replacement-owner-delivery-during-pending",
                "audio-manual-fresh-decode",
                "audio-automatic-fresh-decode",
                "cross-lane-pressure-before-repair",
            ],
        ),
        (
            "admission-regression",
            saved,
            vec![
                "topic-ordered-fresh-replacement-owner-delivery",
                "planned-admission-retry",
            ],
        ),
    ];
    for (label, history, claims) in cases {
        let seed = sim_seed();
        let path = output_dir().join(format!("replay-{label}.history"));
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
        assert!(output.contains("[history-replay]"));
        for claim in claims {
            assert!(
                output.contains(claim),
                "missing replay claim {claim}: {output}"
            );
        }
        assert!(
            !output.contains("[simulation-generated] family=intent-histories"),
            "direct replay regenerated cases"
        );
        assert!(output.contains(&format!("applied={}", history.ops.len())));
    }
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
fn audio_history_rejects_invalid_targets_unsupported_delays_and_vacuous_progress() {
    let base = History {
        claim: Claim::Exploration,
        ops: vec![Op::Join(0)],
    };
    for (pinned, automatic, fixed_ms) in [
        (vec![0], false, Some(0)),
        (vec![0], false, Some(100)),
        (vec![0], false, Some(1001)),
        (vec![4], false, None),
        (vec![0, 0], true, None),
    ] {
        let mut invalid = base.clone();
        invalid.ops.push(Op::Audio {
            pinned,
            automatic,
            fixed_ms,
        });
        assert!(invalid.validate().is_err());
    }
    let mut empty = base.clone();
    empty.ops.extend([
        Op::Audio {
            pinned: vec![],
            automatic: false,
            fixed_ms: None,
        },
        Op::AudioProgress,
    ]);
    assert!(empty.validate().is_err());
    let mut retired = base;
    retired.ops.extend([
        Op::Audio {
            pinned: vec![1],
            automatic: false,
            fixed_ms: None,
        },
        Op::Leave(0),
        Op::AudioProgress,
    ]);
    assert!(retired.validate().is_err());
}

#[test]
fn concrete_format_and_reduction_preserve_validity() {
    for original in forced() {
        let mut composed = original.clone();
        if matches!(composed.claim, Claim::Replacement | Claim::Generation) {
            composed = composed.with_initial_source_replacement();
            let mut no_replacement = composed.clone();
            no_replacement
                .ops
                .retain(|op| !matches!(op, Op::ReplaceSource(_)));
            assert!(no_replacement.validate().is_err());
        }
        composed = composed.with_data_lane().with_audio_lane();
        let mut no_declaration = composed.clone();
        no_declaration
            .ops
            .retain(|op| !matches!(op, Op::DeclareData(_)));
        assert!(no_declaration.validate().is_err());
        for history in [original, composed] {
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
    }
    assert!(History::decode("pulsebeam-history-v2\n").is_err());
    assert!(History::decode("pulsebeam-history-v1\n").is_err());
    assert!(
        History {
            claim: Claim::Exploration,
            ops: vec![Op::Leave(0)]
        }
        .validate()
        .is_err()
    );
    let mut runner = TestRunner::deterministic();
    let mut deliveries = BTreeSet::new();
    for _ in 0..100 {
        let history = strategy()
            .new_tree(&mut runner)
            .unwrap()
            .current()
            .with_audio_lane();
        history.validate().unwrap();
        assert_eq!(
            History::decode(&history.encode(17, &Witness::default())).unwrap(),
            (17, history.clone())
        );
        for op in history.ops {
            if let Op::DeclareData(mode) = op {
                deliveries.insert(mode);
            }
        }
    }
    assert_eq!(
        deliveries,
        BTreeSet::from([Delivery::Latest, Delivery::Lossy, Delivery::Ordered])
    );
}
