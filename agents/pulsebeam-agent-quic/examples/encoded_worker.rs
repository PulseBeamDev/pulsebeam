//! Pinned encoded media worker. No audio or video codecs are loaded.

use std::{
    env, fs,
    future::pending,
    io::{self, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use pulsebeam_agent_core::{
    AgentCommand, AgentConfig, AudioSubscription, ConnectionState, DesiredState, MediaKind,
    RtpSource, VideoSubscription,
};
use pulsebeam_agent_quic::{
    executor::{
        host::Agent,
        recorder::{Limits, Recorder},
        rtp::Binding,
        timed::{Policy, Reader, Receive},
    },
    media::units::{Codec, Event, Unit},
};
use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, pem::PemObject},
};
use tokio::sync::{mpsc, oneshot};

const USAGE: &str = "encoded_worker ENDPOINT ROOM opus|h264|vp8|vp9|av1 PUBLICATION_ID QUIC_CA_PEM fresh\n\
encoded_worker ENDPOINT ROOM opus|h264|vp8|vp9|av1 PUBLICATION_ID QUIC_CA_PEM record NEW_DIRECTORY\n\
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
    publication: String,
    codec: Codec,
    ca: PathBuf,
    record: Option<PathBuf>,
}

impl Settings {
    fn parse(arguments: &[String]) -> Result<Self> {
        let [endpoint, room, codec, publication, ca, mode, output @ ..] = arguments else {
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
            publication: publication.clone(),
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
    input: mpsc::Sender<Work>,
    done: oneshot::Receiver<Result<()>>,
}

fn record_storage(directory: PathBuf, publication: String, codec: Codec) -> Result<Storage> {
    let (input, mut queue) = mpsc::channel::<Work>(4);
    let (finished, done) = oneshot::channel();
    std::thread::Builder::new()
        .name("encoded-storage".into())
        .spawn(move || {
            let result = (|| {
                let mut recorder = Recorder::create(&directory, publication, codec, RECORD_LIMITS)
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
    Ok(Storage { input, done })
}

#[derive(Default)]
struct Processed {
    units: u64,
    bytes: u64,
    discontinuities: u64,
}

impl Processed {
    // Replace this bounded callback with work on complete encoded bytes.
    // Offload slow processing just as the recording mode offloads writes.
    fn process(&mut self, unit: &Unit<Binding>) {
        self.units = self.units.saturating_add(1);
        self.bytes = self.bytes.saturating_add(unit.data.len() as u64);
    }
}

async fn drive(
    agent: &mut Agent,
    mut reader: Reader,
    codec: Codec,
    storage: Option<&mpsc::Sender<Work>>,
) -> Result<Processed> {
    let stop = tokio::signal::ctrl_c();
    tokio::pin!(stop);
    let mut processed = Processed::default();
    loop {
        if let Some(failure) = &agent.snapshot().terminal_failure {
            return Err(format!("connection failed: {:?}", failure.class));
        }
        reader
            .sync(agent)
            .map_err(|error| format!("attach reader: {error:?}"))?;
        let mut blocked = false;
        for _ in 0..34 {
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
                let Some(work) = reader
                    .next(agent)
                    .map_err(|error| format!("attach reader: {error:?}"))?
                else {
                    break;
                };
                permit.send(work);
            } else {
                let Some((_, event)) = reader
                    .next(agent)
                    .map_err(|error| format!("attach reader: {error:?}"))?
                else {
                    break;
                };
                match event {
                    Receive::Media(Event::Unit(unit)) => {
                        if unit.codec != codec {
                            return Err(format!(
                                "expected {codec:?} publication, received {:?}",
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
    let mime = match settings.codec {
        Codec::Opus => "audio/opus",
        Codec::H264 => "video/h264",
        Codec::Vp8 => "video/vp8",
        Codec::Vp9 => "video/vp9",
        Codec::Av1 => "video/av1",
    };
    let source =
        RtpSource::native_quic_baseline(settings.publication.clone(), settings.room.clone(), mime)
            .ok_or("unknown source format")?;
    let mut desired = DesiredState {
        revision: 1,
        rtp_sources: vec![source],
        ..Default::default()
    };
    let kind = match settings.codec {
        Codec::Opus => {
            desired.audio = AudioSubscription {
                pinned: vec![settings.publication.clone()],
                ..Default::default()
            };
            MediaKind::Audio
        }
        Codec::H264 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 => {
            desired.video = vec![VideoSubscription {
                decode_target: None,
                slot: 0,
                publication_id: settings.publication.clone(),
                selector: None,
                height: 720,
                min_height: 0,
                min_fps: 0,
                priority: 0,
            }];
            MediaKind::Video
        }
    };
    let (policy, storage) = if let Some(directory) = settings.record {
        (
            Policy::Recording {
                reorder: Duration::from_millis(120),
            },
            Some(record_storage(
                directory,
                settings.publication.clone(),
                settings.codec,
            )?),
        )
    } else {
        (
            Policy::Freshness {
                reorder: Duration::from_millis(35),
                max_age: Duration::from_millis(150),
            },
            None,
        )
    };
    agent
        .command(AgentCommand::ReplaceDesired(desired))
        .map_err(|error| format!("pin publication: {error:?}"))?;
    agent
        .command(AgentCommand::Join(settings.room))
        .map_err(|error| format!("join: {error:?}"))?;
    let reader = Reader::new(kind, settings.publication, policy);
    let recording = storage.is_some();
    let drive = drive(
        &mut agent,
        reader,
        settings.codec,
        storage.as_ref().map(|storage| &storage.input),
    )
    .await;
    let output = async {
        if let Some(Storage { input, done }) = storage {
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
            "encoded units processed={} bytes={} discontinuities={}",
            processed.units, processed.bytes, processed.discontinuities
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
