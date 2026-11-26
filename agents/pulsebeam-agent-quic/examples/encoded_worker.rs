//! Read a logical source as encoded media without loading a decoder.

use std::{
    env, fs,
    future::pending,
    io::{self, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use pulsebeam_agent_core::{
    AgentCommand, AgentConfig, ConnectionState, MediaKind, SelectorStatus, TrackFilter, TrackOwner,
};
use pulsebeam_agent_quic::{
    executor::{
        host::Agent,
        recorder::{Limits, Recorder},
        rtp::Binding,
        timed::{Policy, Receive},
    },
    media::units::{Codec, Event, Unit},
};
use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, pem::PemObject},
};
use tokio::sync::{mpsc, oneshot};

const USAGE: &str = "encoded_worker ENDPOINT ROOM PARTICIPANT TRACK opus|h264|vp8|vp9|av1 QUIC_CA_PEM fresh\n\
encoded_worker ENDPOINT ROOM PARTICIPANT TRACK opus|h264|vp8|vp9|av1 QUIC_CA_PEM record NEW_DIRECTORY\n\
Set PULSEBEAM_TOKEN and PULSEBEAM_LOCAL_IP. Optional: PULSEBEAM_PLACEMENT_CA_DER.";
const DRAIN_GRACE: Duration = Duration::from_secs(3);
const RECORD_LIMITS: Limits = Limits {
    units: 100_000,
    bytes: 1024 * 1024 * 1024,
};

type Result<T> = std::result::Result<T, String>;
type Work = (Binding, Receive);

struct Settings {
    endpoint: String,
    room: String,
    participant: String,
    track: String,
    codec: Codec,
    ca: PathBuf,
    record: Option<PathBuf>,
}

impl Settings {
    fn parse(arguments: &[String]) -> Result<Self> {
        let [
            endpoint,
            room,
            participant,
            track,
            codec,
            ca,
            mode,
            output @ ..,
        ] = arguments
        else {
            return Err(USAGE.into());
        };
        let codec = match codec.as_str() {
            "opus" => Codec::Opus,
            "h264" => Codec::H264,
            "vp8" => Codec::Vp8,
            "vp9" => Codec::Vp9,
            "av1" => Codec::Av1,
            _ => return Err(USAGE.into()),
        };
        let record = match (mode.as_str(), output) {
            ("fresh", []) => None,
            ("record", [directory]) => Some(PathBuf::from(directory)),
            _ => return Err(USAGE.into()),
        };
        Ok(Self {
            endpoint: endpoint.clone(),
            room: room.clone(),
            participant: participant.clone(),
            track: track.clone(),
            codec,
            ca: ca.into(),
            record,
        })
    }
}

fn roots(path: &Path) -> Result<RootCertStore> {
    let certs =
        CertificateDer::pem_file_iter(path).map_err(|error| format!("read QUIC CA: {error}"))?;
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots
            .add(cert.map_err(|error| format!("parse QUIC CA: {error}"))?)
            .map_err(|error| format!("install QUIC CA: {error}"))?;
    }
    if roots.is_empty() {
        return Err("QUIC CA file did not contain a certificate".into());
    }
    Ok(roots)
}

struct Storage {
    publication: String,
    input: mpsc::Sender<Work>,
    done: oneshot::Receiver<Result<()>>,
}

fn record_storage(directory: PathBuf, publication: String, codec: Codec) -> Result<Storage> {
    let (input, mut queue) = mpsc::channel::<Work>(4);
    let (finished, done) = oneshot::channel();
    let recorded_publication = publication.clone();
    std::thread::Builder::new()
        .name("encoded-storage".into())
        .spawn(move || {
            let result = (|| {
                let mut recorder =
                    Recorder::create(&directory, recorded_publication, codec, RECORD_LIMITS)
                        .map_err(|error| format!("open recording: {error:?}"))?;
                while let Some((binding, event)) = queue.blocking_recv() {
                    recorder
                        .record(binding, &event)
                        .map_err(|error| format!("record encoded event: {error:?}"))?;
                }
                Ok(())
            })();
            let _ = finished.send(result);
        })
        .map_err(|error| format!("start storage: {error}"))?;
    Ok(Storage {
        publication,
        input,
        done,
    })
}

#[derive(Default)]
struct Processed {
    binding: Option<Binding>,
    units: u64,
    bytes: u64,
    discontinuities: u64,
}

impl Processed {
    fn process(&mut self, unit: &Unit<Binding>) {
        self.units = self.units.saturating_add(1);
        self.bytes = self.bytes.saturating_add(unit.data.len() as u64);
    }
}

struct Worker {
    owner: TrackOwner,
    codec: Codec,
    policy: Policy,
    directory: Option<PathBuf>,
    storage: Option<Storage>,
    binding: Option<Binding>,
}

impl Worker {
    fn sync(&mut self, agent: &mut Agent) -> Result<Option<Binding>> {
        let state = agent
            .media_selector_state(self.owner)
            .ok_or("selector closed")?;
        match state.status {
            SelectorStatus::Rejected => {
                return Err(format!("selector rejected: {:?}", state.error));
            }
            SelectorStatus::Overflow => return Err("selector exceeded membership capacity".into()),
            SelectorStatus::Pending | SelectorStatus::Complete => {}
        }
        let selected = match state.matches.as_slice() {
            [] => None,
            [matched] => agent
                .selected_rtp(self.owner, matched)
                .map_err(|error| format!("receive selected track: {error:?}"))?,
            _ => return Err("selector returned more than one logical source".into()),
        };
        if selected.map(|selected| selected.binding) == self.binding {
            return Ok(self.binding);
        }
        let selected = selected.map(|live| (live.binding, live.publication_id.to_owned()));
        if let Some(previous) = self.binding.take() {
            agent.detach_encoded(previous);
        }
        let Some((binding, publication)) = selected else {
            return Ok(None);
        };
        if let Some(storage) = &self.storage {
            if storage.publication != publication {
                return Err("publication replaced; use a new recording directory".into());
            }
        } else if let Some(directory) = self.directory.take() {
            self.storage = Some(record_storage(directory, publication, self.codec)?);
        }
        agent
            .attach_encoded(binding, self.policy)
            .map_err(|error| format!("attach reader: {error:?}"))?;
        self.binding = Some(binding);
        Ok(self.binding)
    }
}

async fn drive(agent: &mut Agent, worker: &mut Worker) -> Result<Processed> {
    let stop = tokio::signal::ctrl_c();
    tokio::pin!(stop);
    let mut processed = Processed::default();
    loop {
        if let Some(failure) = &agent.snapshot().terminal_failure {
            return Err(format!("connection failed: {:?}", failure.class));
        }
        let binding = worker.sync(agent)?;
        if binding.is_some() && binding != processed.binding {
            processed = Processed {
                binding,
                ..Default::default()
            };
        }
        let storage = worker.storage.as_ref().map(|storage| &storage.input);
        if storage.is_some_and(mpsc::Sender::is_closed) {
            return Err("storage stopped".into());
        }
        let mut blocked = false;
        for binding in binding.into_iter().cycle().take(34) {
            if let Some(storage) = storage {
                let permit = match storage.try_reserve() {
                    Ok(permit) => permit,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        blocked = true;
                        break;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        return Err("storage stopped".into());
                    }
                };
                let Some(event) = agent.next_encoded(binding) else {
                    break;
                };
                permit.send((binding, event));
            } else {
                let Some(event) = agent.next_encoded(binding) else {
                    break;
                };
                match event {
                    Receive::Media(Event::Unit(unit)) => {
                        if unit.source != binding || unit.codec != worker.codec {
                            return Err(format!(
                                "unexpected source or codec on {binding:?}: {:?}",
                                unit.codec
                            ));
                        }
                        processed.process(&unit);
                    }
                    Receive::Media(Event::Discontinuity(_)) | Receive::Loss(_) => {
                        processed.discontinuities = processed.discontinuities.saturating_add(1);
                    }
                }
            }
        }
        let resume_storage = async {
            if let Some(storage) = storage {
                if blocked {
                    drop(storage.reserve().await);
                } else {
                    storage.closed().await;
                }
            } else {
                pending::<()>().await;
            }
        };
        tokio::select! {
            result = &mut stop => {
                result.map_err(|error| format!("shutdown signal: {error}"))?;
                return Ok(processed);
            }
            () = resume_storage => {},
            result = agent.turn() => result.map_err(|error| format!("drive Agent: {error:?}"))?,
        }
    }
}

async fn shutdown(agent: &mut Agent) -> Result<()> {
    agent
        .command(AgentCommand::Disconnect)
        .map_err(|error| format!("disconnect: {error:?}"))?;
    tokio::time::timeout(DRAIN_GRACE, async {
        while agent.snapshot().connection != ConnectionState::Disconnected {
            agent
                .turn()
                .await
                .map_err(|error| format!("settle disconnect: {error:?}"))?;
        }
        Ok(())
    })
    .await
    .map_err(|_| "disconnect deadline".to_owned())?
}

async fn run(settings: Settings) -> Result<()> {
    let token = env::var("PULSEBEAM_TOKEN").map_err(|_| "set PULSEBEAM_TOKEN".to_owned())?;
    let local: IpAddr = env::var("PULSEBEAM_LOCAL_IP")
        .map_err(|_| "set PULSEBEAM_LOCAL_IP to this host's local IP address".to_owned())?
        .parse()
        .map_err(|_| "invalid PULSEBEAM_LOCAL_IP".to_owned())?;
    if local.is_unspecified() {
        return Err("PULSEBEAM_LOCAL_IP must be a concrete address in the edge's IP family".into());
    }
    let mut agent = Agent::new(
        AgentConfig {
            endpoint: settings.endpoint,
            token,
            retry: Default::default(),
            log_level: Default::default(),
        },
        local,
        roots(&settings.ca)?,
    )
    .map_err(|error| format!("configure Agent: {error:?}"))?;
    if let Some(path) = env::var_os("PULSEBEAM_PLACEMENT_CA_DER") {
        let der = fs::read(path).map_err(|error| format!("read placement CA: {error}"))?;
        agent = agent
            .with_placement_root(&der)
            .map_err(|error| format!("install placement CA: {error:?}"))?;
    }
    let kind = match settings.codec {
        Codec::Opus => MediaKind::Audio,
        Codec::H264 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 => MediaKind::Video,
    };
    let policy = if settings.record.is_some() {
        Policy::Recording {
            reorder: Duration::from_millis(120),
        }
    } else {
        Policy::Freshness {
            reorder: Duration::from_millis(35),
            max_age: Duration::from_millis(150),
        }
    };
    let owner = agent
        .open_media_selector(
            settings.room.clone(),
            kind,
            TrackFilter {
                participant_id: Some(settings.participant),
                track_id: Some(settings.track),
            },
        )
        .map_err(|error| format!("open selector: {error:?}"))?;
    agent
        .command(AgentCommand::Join(settings.room))
        .map_err(|error| format!("join: {error:?}"))?;
    let mut worker = Worker {
        owner,
        codec: settings.codec,
        policy,
        directory: settings.record,
        storage: None,
        binding: None,
    };
    let drive = drive(&mut agent, &mut worker).await;
    if let Some(binding) = worker.binding {
        agent.detach_encoded(binding);
    }
    agent.close_media_selector(owner);
    let recording = worker.storage.is_some();
    let output = async {
        if let Some(Storage { input, done, .. }) = worker.storage {
            drop(input);
            tokio::time::timeout(DRAIN_GRACE, done)
                .await
                .map_err(|_| "recording drain deadline".to_owned())?
                .map_err(|_| "storage thread exited without a result".to_owned())??;
        }
        Ok::<_, String>(())
    };
    let (closed, recorded) = tokio::join!(shutdown(&mut agent), output);
    recorded?;
    let processed = drive?;
    closed?;
    let mut stderr = io::stderr().lock();
    if recording {
        let _ = writeln!(stderr, "recording closed");
    } else {
        let _ = writeln!(
            stderr,
            "binding {:?}: encoded units={} bytes={} discontinuities={}",
            processed.binding, processed.units, processed.bytes, processed.discontinuities
        );
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.as_slice() == ["--help"] {
        let _ = writeln!(io::stdout().lock(), "{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    let result = match Settings::parse(&args) {
        Ok(settings) => run(settings).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
