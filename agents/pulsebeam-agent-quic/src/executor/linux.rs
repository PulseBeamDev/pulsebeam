use std::{
    collections::BTreeMap,
    future::{Future, pending},
    net::IpAddr,
    pin::Pin,
    time::Duration,
};

use noq_proto::ClientConfig;
use pulsebeam_agent_core::{
    Agent, AgentCommand, AgentConfig, AgentError, Effect, Failure, FailureClass, Generation,
    HostEvent, HttpEffect, HttpEvent, HttpMethod, HttpRequest, HttpResponse, Notification,
    OperationId, QuicEffect, QuicEvent, Snapshot, TimerEffect, TimerEvent, TimerId,
};
use pulsebeam_runtime::time::{self, Instant};
use rustls::RootCertStore;

use crate::transport;

use super::udp::{OpenError, UdpAssociation};

const PLACEMENT_BODY_LIMIT: usize = 1_024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const EFFECTS_PER_TURN: usize = 32;

type Work<T> = Pin<Box<dyn Future<Output = T>>>;
type DialResult = Result<Box<UdpAssociation>, QuicEvent>;

#[derive(Debug)]
pub enum Error {
    Core(AgentError),
    Http(reqwest::Error),
    Tls(transport::ConfigError),
    UnexpectedEffect,
    Capacity,
}

/// Caller-driven Linux Agent. Run its turns on the reactor that owns the UDP
/// sockets. It does not dispatch a background task or retain an RTC host.
pub struct LinuxAgent {
    core: Agent,
    http: reqwest::Client,
    tls: ClientConfig,
    local: IpAddr,
    clock_origin: Instant,
    request: Option<(OperationId, Work<HttpEvent>)>,
    dialing: Option<(Generation, Work<DialResult>)>,
    sockets: [Option<Box<UdpAssociation>>; 2],
    timers: BTreeMap<TimerId, Instant>,
}

impl LinuxAgent {
    pub fn new(
        config: AgentConfig,
        local: IpAddr,
        quic_roots: RootCertStore,
    ) -> Result<Self, Error> {
        let core = Agent::new_quic(config).map_err(Error::Core)?;
        let http = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(Error::Http)?;
        let tls = transport::system_client_config(quic_roots).map_err(Error::Tls)?;
        Ok(Self {
            core,
            http,
            tls,
            local,
            clock_origin: Instant::now(),
            request: None,
            dialing: None,
            sockets: [None, None],
            timers: BTreeMap::new(),
        })
    }

    pub fn command(&mut self, command: AgentCommand) -> Result<(), AgentError> {
        self.core.command_at(command, self.clock_origin.elapsed())
    }

    pub fn snapshot(&self) -> &Snapshot {
        self.core.snapshot()
    }

    pub fn next_notification(&mut self) -> Option<Notification> {
        self.core.next_notification()
    }

    /// Execute bounded ready effects, then wait for one input. It is safe to
    /// cancel this wait, invoke a command, and drive the same Agent again.
    pub async fn turn(&mut self) -> Result<(), Error> {
        for _ in 0..EFFECTS_PER_TURN {
            let Some(effect) = self.core.next_effect() else {
                return self.wait().await;
            };
            self.execute(effect)?;
        }
        Ok(())
    }

    fn accept(&mut self, event: HostEvent) -> Result<(), Error> {
        self.core
            .handle_at(event, self.clock_origin.elapsed())
            .map_err(Error::Core)
    }

    fn execute(&mut self, effect: Effect) -> Result<(), Error> {
        match effect {
            Effect::Http(HttpEffect::Request {
                operation, request, ..
            }) => {
                if self.request.is_some() {
                    return Err(Error::Capacity);
                }
                let client = self.http.clone();
                self.request = Some((
                    operation,
                    Box::pin(async move {
                        match execute_placement(&client, request).await {
                            Some(response) => HttpEvent::Response {
                                operation,
                                response,
                            },
                            None => HttpEvent::Failed {
                                operation,
                                message: "HTTPS placement failed".into(),
                            },
                        }
                    }),
                ));
            }
            Effect::Http(HttpEffect::Cancel { operation }) => {
                if self
                    .request
                    .as_ref()
                    .is_some_and(|(id, _)| *id == operation)
                {
                    self.request = None;
                }
            }
            Effect::Timer(TimerEffect::Schedule { timer, after }) => {
                let now = Instant::now();
                self.timers
                    .insert(timer, now.checked_add(after).unwrap_or(now));
            }
            Effect::Timer(TimerEffect::Cancel { timer }) => {
                self.timers.remove(&timer);
            }
            Effect::Quic(QuicEffect::Admit(admit)) => {
                if self.dialing.is_some() || self.sockets.iter().all(Option::is_some) {
                    return Err(Error::Capacity);
                }
                let generation = admit.generation;
                let tls = self.tls.clone();
                let local = self.local;
                let origin = self.clock_origin;
                self.dialing = Some((
                    generation,
                    Box::pin(async move {
                        let unavailable = || QuicEvent::AdmissionFailed {
                            generation,
                            failure: transient("QUIC authority unavailable"),
                        };
                        let deadline =
                            origin.checked_add(admit.deadline).ok_or_else(unavailable)?;
                        let connect = async {
                            let remote = tokio::net::lookup_host((
                                admit.placement.host.as_str(),
                                admit.placement.port.get(),
                            ))
                            .await
                            .map_err(|_| unavailable())?
                            .find(|address| address.is_ipv4() == local.is_ipv4())
                            .ok_or_else(unavailable)?;
                            UdpAssociation::connect(
                                origin.into_std(),
                                local,
                                remote.ip(),
                                tls,
                                admit,
                            )
                            .await
                            .map(Box::new)
                            .map_err(|error| match error {
                                OpenError::Admission(event) => event,
                                OpenError::Socket(_) => unavailable(),
                            })
                        };
                        time::timeout_at(deadline, connect)
                            .await
                            .map_err(|_| unavailable())?
                    }),
                ));
            }
            Effect::Quic(effect @ QuicEffect::Close { generation }) => {
                if self
                    .dialing
                    .as_ref()
                    .is_some_and(|(id, _)| *id == generation)
                {
                    self.dialing = None;
                }
                if let Some(socket) = self
                    .sockets
                    .iter_mut()
                    .flatten()
                    .find(|socket| socket.generation() == generation)
                {
                    socket.submit(effect).map_err(|_| Error::UnexpectedEffect)?;
                } else {
                    self.accept(HostEvent::Quic(QuicEvent::Closed { generation }))?;
                }
            }
            Effect::Quic(effect @ (QuicEffect::SendControl { .. } | QuicEffect::Renew(_))) => {
                let generation = match &effect {
                    QuicEffect::SendControl { generation, .. } => *generation,
                    QuicEffect::Renew(renew) => renew.generation,
                    _ => return Err(Error::UnexpectedEffect),
                };
                if let Some(slot) = self.sockets.iter_mut().find(|socket| {
                    socket
                        .as_ref()
                        .is_some_and(|socket| socket.generation() == generation)
                }) && let Some(socket) = slot.as_mut()
                    && socket.submit(effect).is_err()
                {
                    let event = socket.io_failure();
                    *slot = None;
                    self.accept(HostEvent::Quic(event))?;
                }
            }
            _ => return Err(Error::UnexpectedEffect),
        }
        Ok(())
    }

    async fn wait(&mut self) -> Result<(), Error> {
        let next_timer = self
            .timers
            .iter()
            .min_by_key(|(_, deadline)| *deadline)
            .map(|(timer, at)| (*timer, *at));
        let timer = async {
            if let Some((timer, at)) = next_timer {
                time::sleep_until(at).await;
                timer
            } else {
                pending().await
            }
        };
        let [first, second] = &mut self.sockets;
        let input = pulsebeam_runtime::select! {
            http = work(&mut self.request) => Input::Http(http),
            dial = work(&mut self.dialing) => Input::Dial(dial),
            timer = timer => Input::Timer(timer),
            event = socket_turn(first) => Input::Socket(event),
            event = socket_turn(second) => Input::Socket(event),
        };
        match input {
            Input::Http(event) => {
                self.request = None;
                self.accept(HostEvent::Http(event))?;
            }
            Input::Dial(result) => {
                self.dialing = None;
                match result {
                    Ok(socket) => {
                        let slot = self
                            .sockets
                            .iter_mut()
                            .find(|socket| socket.is_none())
                            .ok_or(Error::Capacity)?;
                        *slot = Some(socket);
                    }
                    Err(event) => self.accept(HostEvent::Quic(event))?,
                }
            }
            Input::Timer(timer) => {
                self.timers.remove(&timer);
                self.accept(HostEvent::Timer(TimerEvent::Fired { timer }))?;
            }
            Input::Socket(Some(event)) => self.accept(HostEvent::Quic(event))?,
            Input::Socket(None) => {}
        }
        Ok(())
    }
}

enum Input {
    Http(HttpEvent),
    Dial(DialResult),
    Timer(TimerId),
    Socket(Option<QuicEvent>),
}

async fn work<Id, T>(work: &mut Option<(Id, Work<T>)>) -> T {
    match work {
        Some((_, future)) => future.as_mut().await,
        None => pending().await,
    }
}

async fn socket_turn(slot: &mut Option<Box<UdpAssociation>>) -> Option<QuicEvent> {
    let Some(socket) = slot.as_mut() else {
        return pending().await;
    };
    let Ok(event) = socket.progress().await else {
        let event = socket.io_failure();
        *slot = None;
        return Some(event);
    };
    if matches!(event, Some(QuicEvent::Closed { .. })) {
        *slot = None;
    }
    event
}

async fn execute_placement(http: &reqwest::Client, request: HttpRequest) -> Option<HttpResponse> {
    if request.method != HttpMethod::Post {
        return None;
    }
    let mut outgoing = http.post(request.uri).body(request.body);
    for header in request.headers {
        outgoing = outgoing.header(header.name, header.value);
    }
    let mut response = outgoing.send().await.ok()?;
    let status = response.status().as_u16();
    let mut body = Vec::new();
    if status == 200 {
        while let Some(chunk) = response.chunk().await.ok()? {
            if body.len().saturating_add(chunk.len()) > PLACEMENT_BODY_LIMIT {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
    }
    Some(HttpResponse {
        status,
        body,
        headers: Vec::new(),
    })
}

fn transient(message: &'static str) -> Failure {
    Failure {
        class: FailureClass::Transient,
        message: message.into(),
    }
}
