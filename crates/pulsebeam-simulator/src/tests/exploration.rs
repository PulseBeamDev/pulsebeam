//! One materialized execution per process. The existing runner owns complete
//! prefix traversal and isolation; seeds are not substitutes for these choices.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Duration};
use turmoil::exploration::{Decision, Event};

use super::common::{LinkProfile, LocalNodeSim, Participant, Room, Step};

mod boundaries;
mod crypto;
mod replay;

pub const MODEL: &str = "cross-shard-ordered-v1";
pub const SEED: u64 = 0xDEAD_BEEF;
pub const TOPIC: &str = "bounded-ordered";
pub const NAMES: [&str; 4] = ["publisher", "receiver1", "receiver2", "bystander"];
pub const ORDERS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub site: String,
    pub enabled: Vec<String>,
    pub selected: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub model: String,
    pub seed: u64,
    pub participants: Vec<String>,
    pub room: String,
    pub topic: String,
    pub link: String,
    pub publisher_rejoins: usize,
    pub failure_limit: usize,
    pub sctp_initial_rto_ms: u64,
    pub shards: usize,
    pub subnet: u8,
    pub horizon_ms: u64,
    pub quantum_us: u64,
    pub delayed_udp_ms: u64,
    pub stall_ms: u64,
    pub payloads: Vec<Vec<u8>>,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            model: MODEL.into(),
            seed: SEED,
            participants: NAMES.map(String::from).to_vec(),
            room: "bounded".into(),
            topic: TOPIC.into(),
            link: format!("{:?}", LinkProfile::fiber()),
            publisher_rejoins: 1,
            failure_limit: 1,
            // sctp-proto 0.10.4 DATA T3-RTX initial RTO. Delayed branches also
            // require delivery by retransmission before releasing the original.
            sctp_initial_rto_ms: 3000,
            shards: 2,
            subnet: 71,
            horizon_ms: 60_000,
            quantum_us: 1000,
            delayed_udp_ms: 3100,
            stall_ms: 100,
            payloads: (0..4)
                .map(|n| {
                    let mut bytes = vec![b'a' + n; 4096];
                    bytes[..12].copy_from_slice(b"publisher-v1");
                    bytes[12] = n;
                    bytes
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub scenario: Scenario,
    pub build: String,
    pub dependencies: String,
    pub fixture: String,
    pub choices: Vec<Choice>,
    pub observations: Vec<String>,
    pub consumed: Vec<String>,
    pub disabled: Vec<String>,
    pub outcome: String,
    pub diagnostic: Option<String>,
}

pub struct Execution {
    pub artifact: Artifact,
    prefix: Vec<Choice>,
    replay: Option<Artifact>,
    pub host: Option<(&'static str, bool)>,
    pub traffic: bool,
    pub timing: bool,
    pub pair: bool,
    pub fault: usize,
    pub network_fault: bool,
    pub latent: Option<String>,
    pub delayed: bool,
    release_step: usize,
    host_step: usize,
}

thread_local! {
    static EXECUTION: RefCell<Option<Rc<RefCell<Execution>>>> = const { RefCell::new(None) };
}

pub fn with<T>(f: impl FnOnce(&mut Execution) -> T) -> T {
    EXECUTION.with_borrow(|state| {
        f(&mut state
            .as_ref()
            .expect("bounded execution installed")
            .borrow_mut())
    })
}

impl Execution {
    fn new(build: String, prefix: Vec<Choice>, replay: Option<Artifact>) -> Self {
        let scenario = Scenario::default();
        let mut execution = Execution {
            artifact: Artifact {
                scenario: scenario.clone(),
                build: build.clone(),
                dependencies: format!(
                    "{:x}",
                    Sha256::digest(concat!(
                        include_str!("../../../../Cargo.lock"),
                        include_str!("../../../../tools/patches/turmoil-bounded-exploration.patch"),
                        include_str!("../../../../tools/patches/getrandom-simulator-libc.patch"),
                    ))
                ),
                fixture: format!(
                    "{:x}",
                    Sha256::new()
                        .chain_update(serde_json::to_vec(&scenario).unwrap())
                        .chain_update(include_bytes!("exploration/fixtures/cert.der"))
                        .chain_update(include_bytes!("exploration/fixtures/key.der"))
                        .finalize()
                ),
                choices: Vec::new(),
                observations: Vec::new(),
                consumed: Vec::new(),
                disabled: Vec::new(),
                outcome: "satisfied".into(),
                diagnostic: None,
            },
            prefix: replay
                .as_ref()
                .map_or(prefix, |artifact| artifact.choices.clone()),
            replay: replay.clone(),
            host: None,
            traffic: false,
            timing: false,
            pair: false,
            fault: 0,
            network_fault: false,
            latent: None,
            delayed: false,
            release_step: 0,
            host_step: 0,
        };
        if let Some(recorded) = &replay
            && (recorded.scenario != scenario
                || recorded.build != build
                || recorded.dependencies != execution.artifact.dependencies
                || recorded.fixture != execution.artifact.fixture)
        {
            execution.reject(
                "replay-divergence",
                "incompatible model, materialized inputs or build identity".into(),
            );
        }

        execution
    }

    fn finish(&mut self) {
        if self.artifact.choices.len() < self.prefix.len() {
            self.reject("replay-divergence", "unconsumed recorded choices".into());
        }
        if let Some(recorded) = &self.replay
            && &self.artifact != recorded
        {
            self.reject(
                "replay-divergence",
                "changed observations, fault consumption, choices or semantic outcome".into(),
            );
        }
    }

    fn reject(&mut self, kind: &str, diagnostic: String) {
        if self.artifact.diagnostic.is_none() || kind == "replay-divergence" {
            self.artifact.outcome = kind.into();
            self.artifact.diagnostic = Some(diagnostic);
        }
    }

    pub fn invalid(&mut self, message: &str) {
        self.reject("invalid-model", message.into());
    }

    pub fn choose(&mut self, site: impl Into<String>, enabled: Vec<String>) -> usize {
        let site = site.into();
        if enabled.is_empty() {
            self.reject("invalid-model", format!("empty alternatives at {site}"));
            return 0;
        }
        let index = self.artifact.choices.len();
        let selected = match self.prefix.get(index) {
            Some(expected)
                if expected.site == site
                    && expected.enabled == enabled
                    && expected.selected < enabled.len() =>
            {
                expected.selected
            }
            Some(expected) => {
                self.reject(
                    "replay-divergence",
                    format!("choice {index}: expected {expected:?}, reached {site} {enabled:?}"),
                );
                0
            }
            None if self.replay.is_some() => {
                self.reject(
                    "replay-divergence",
                    format!("extra choice {index} at {site}"),
                );
                0
            }
            None => 0,
        };
        self.artifact.choices.push(Choice {
            site,
            enabled,
            selected,
        });
        selected
    }

    pub fn observe(&mut self, value: String) {
        self.artifact.observations.push(value);
    }

    pub fn consume(&mut self, site: impl Into<String>) {
        self.artifact.consumed.push(site.into());
    }

    fn hook(&mut self, event: Event<'_>) -> Decision {
        match event {
            Event::Hosts { step, runnable } => {
                self.host_step = step;
                let reverse = if let Some((site, live_work)) = self.host.take() {
                    let enabled = if live_work && runnable.len() > 1 {
                        self.observe(format!(
                            "host-eligible/{site}/pending-native-work/step={step}"
                        ));
                        vec!["canonical".into(), "reverse".into()]
                    } else {
                        self.artifact
                            .disabled
                            .push(format!("host/{site}: no live competing work"));
                        vec!["canonical".into()]
                    };
                    self.choose(format!("host/{site}/{step}/{runnable:?}"), enabled) == 1
                } else {
                    false
                };
                Decision::Hosts { reverse }
            }
            Event::Udp { src, dst, bytes }
                if self.traffic
                    && src.ip().to_string().ends_with(".2")
                    && dst.port() == 3478
                    && bytes.len() > 900
                    && bytes.first() == Some(&23) =>
            {
                let id = format!("{src}>{dst}/{}:{:x}", bytes.len(), Sha256::digest(bytes));
                let timing_opportunity = !self.timing;
                let delay = if timing_opportunity {
                    self.timing = true;
                    if self.choose(
                        format!("timing/{id}"),
                        vec!["nominal".into(), "delay-3100ms".into()],
                    ) == 1
                    {
                        Duration::from_millis(self.artifact.scenario.delayed_udp_ms)
                    } else {
                        Duration::ZERO
                    }
                } else {
                    Duration::ZERO
                };
                if !delay.is_zero() {
                    self.delayed = true;
                    self.release_step = self.host_step
                        + usize::try_from(
                            delay.as_micros() / u128::from(self.artifact.scenario.quantum_us),
                        )
                        .unwrap();
                    self.latent = Some(id.clone());
                    self.observe(format!(
                        "delayed/{id}/enqueue-step={}/release-after-step={}",
                        self.host_step, self.release_step
                    ));
                }
                let (drop, duplicate) =
                    if !timing_opportunity && !self.network_fault && matches!(self.fault, 1 | 2) {
                        self.network_fault = true;
                        self.consume(format!(
                            "udp/{}/{id}",
                            if self.fault == 1 { "loss" } else { "duplicate" }
                        ));
                        (self.fault == 1, self.fault == 2)
                    } else {
                        (false, false)
                    };
                Decision::Udp {
                    delay,
                    drop,
                    duplicate,
                }
            }
            Event::UdpPair {
                src,
                dst,
                first,
                second,
            } if self.traffic
                && !self.pair
                && src.ip().to_string().ends_with(".2")
                && dst.port() == 3478
                && first.len() > 900
                && second.len() > 900
                && first.first() == Some(&23)
                && second.first() == Some(&23) =>
            {
                self.pair = true;
                let site = format!(
                    "udp-pair/{src}>{dst}/{:x}/{:x}",
                    Sha256::digest(first),
                    Sha256::digest(second)
                );
                let reverse = self.choose(site, vec!["forward".into(), "reverse".into()]) == 1;
                Decision::UdpPair { reverse }
            }
            Event::UdpDelivered { src, dst, bytes } if self.latent.is_some() => {
                let id = format!("{src}>{dst}/{}:{:x}", bytes.len(), Sha256::digest(bytes));
                if self.latent.as_deref() == Some(&id) {
                    if self.host_step < self.release_step {
                        self.invalid("delayed UDP released before the declared retry boundary");
                    }
                    self.latent = None;
                    self.observe(format!("released/{id}/step={}", self.host_step));
                }
                Decision::Default
            }
            _ => Decision::Default,
        }
    }
}

fn input<T: serde::de::DeserializeOwned>(key: &str) -> anyhow::Result<Option<T>> {
    match std::env::var_os(key) {
        Some(path) => Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?)),
        None => Ok(None),
    }
}

#[test]
fn bounded_cross_shard() {
    crypto::install();
    let scenario = Scenario::default();
    assert!(scenario.delayed_udp_ms > scenario.sctp_initial_rto_ms);
    assert_eq!(
        Duration::from_micros(scenario.quantum_us),
        pulsebeam_runtime::SHARD_TIMER_QUANTUM
    );
    let build = std::env::var("PULSEBEAM_BOUNDED_BUILD")
        .expect("run through //:replay for identity and complete traversal");
    let inputs = input::<Vec<Choice>>("PULSEBEAM_BOUNDED_PREFIX").and_then(|prefix| {
        Ok((
            prefix.unwrap_or_default(),
            input::<Artifact>("PULSEBEAM_BOUNDED_REPLAY")?,
        ))
    });
    let execution = match inputs {
        Ok((prefix, replay)) => Execution::new(build, prefix, replay),
        Err(error) => {
            let mut execution = Execution::new(build, vec![], None);
            let kind = if std::env::var_os("PULSEBEAM_BOUNDED_REPLAY").is_some() {
                "replay-divergence"
            } else {
                "infrastructure-failure"
            };
            execution.reject(kind, format!("cannot read bounded input: {error:#}"));
            execution
        }
    };
    let state = Rc::new(RefCell::new(execution));
    EXECUTION.with_borrow_mut(|slot| *slot = Some(state.clone()));
    let hook_state = state.clone();
    let guard = turmoil::exploration::install(move |event| hook_state.borrow_mut().hook(event));
    if state.borrow().artifact.diagnostic.is_none() {
        let result = std::panic::catch_unwind(|| {
            LocalNodeSim::new()
                .with_rng_seed(SEED)
                .with_subnet(scenario.subnet)
                .with_shards(scenario.shards)
                .with_link(LinkProfile::fiber())
                .with_room(
                    Room::new("bounded")
                        .with_participant(Participant::data_participant(NAMES[0]))
                        .with_participant(Participant::data_participant(NAMES[1]))
                        .with_participant(Participant::data_participant(NAMES[2]))
                        .with_participant(Participant::data_participant(NAMES[3])),
                )
                .run(vec![Step::BoundedFamily]);
        });
        if result.is_err() {
            state.borrow_mut().reject(
                "infrastructure-failure",
                "simulation aborted; see retained process trace".into(),
            );
        }
    }
    drop(guard);
    let mut state = state.borrow_mut();
    state.finish();
    let output = PathBuf::from(
        std::env::var("PULSEBEAM_BOUNDED_OUTPUT").expect("runner supplies artifact path"),
    );
    std::fs::write(&output, serde_json::to_vec_pretty(&state.artifact).unwrap()).unwrap();
    println!(
        "[bounded-execution] {} {}",
        state.artifact.outcome,
        output.display()
    );
    assert_eq!(
        state.artifact.outcome, "satisfied",
        "{:?}",
        state.artifact.diagnostic
    );
}

pub fn counterexample(error: &anyhow::Error) {
    with(|execution| execution.reject("counterexample", format!("{error:#}")));
}
