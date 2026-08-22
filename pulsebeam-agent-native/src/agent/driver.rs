use std::collections::VecDeque;
use std::net::SocketAddr;

use pulsebeam_agent_core::{
    AgentCore, CoreConfig, CoreEffect, CoreError, CoreInput, MonotonicTime, SessionError,
    TransportGeneration,
};
use tokio::net::{TcpStream, UdpSocket};

use crate::clock::ClockAnchor;
use crate::media::{
    BandwidthController, BandwidthEstimate, KeyframeController, MediaError, RtpPacket, RtpRouter,
};
use crate::tcp::{TcpError, TcpSession};

use super::handles::{Agent, AgentEvent, DriverCommand};
use super::mailbox;
use super::session::NativeSession;

pub enum NativeTransport {
    None,
    Udp { socket: UdpSocket, peer: SocketAddr },
    Tcp(TcpSession),
}

impl NativeTransport {
    pub fn udp(socket: UdpSocket, peer: SocketAddr) -> Self {
        Self::Udp { socket, peer }
    }

    pub async fn tcp_connect(addr: SocketAddr) -> Result<Self, AgentError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|error| AgentError::Io(error.to_string()))?;
        Ok(Self::Tcp(TcpSession::new(stream)))
    }
}

pub struct AgentDriver {
    core: AgentCore,
    transport: NativeTransport,
    session: NativeSession,
    router: RtpRouter,
    keyframes: KeyframeController,
    bandwidth: BandwidthController,
    rtc: Option<str0m::Rtc>,
    active_generation: Option<TransportGeneration>,
    events: VecDeque<AgentEvent>,
    clock: ClockAnchor,
}

impl AgentDriver {
    pub fn new(config: CoreConfig, transport: NativeTransport) -> Self {
        Self {
            core: AgentCore::new(config),
            transport,
            session: NativeSession::new(),
            router: RtpRouter::default(),
            keyframes: KeyframeController::default(),
            bandwidth: BandwidthController::default(),
            rtc: None,
            active_generation: None,
            events: VecDeque::new(),
            clock: ClockAnchor::default(),
        }
    }

    pub fn agent(
        &self,
        participant_id: impl Into<pulsebeam_agent_core::ParticipantId>,
    ) -> (Agent, mailbox::Receiver<DriverCommand>) {
        let (sender, receiver) = mailbox::bounded(64);
        (Agent::new(participant_id.into(), sender), receiver)
    }

    pub fn core(&self) -> &AgentCore {
        &self.core
    }

    pub fn session(&self) -> &NativeSession {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut NativeSession {
        &mut self.session
    }

    pub fn set_rtc(&mut self, rtc: str0m::Rtc) {
        self.rtc = Some(rtc);
    }

    pub fn next_deadline(&self) -> Option<MonotonicTime> {
        self.core.next_deadline()
    }

    pub async fn handle(&mut self, now: MonotonicTime, input: CoreInput) -> Result<(), AgentError> {
        if let Err(error) = self.core.handle(now, input) {
            self.events.push_back(AgentEvent::Error(error.clone()));
            return Err(AgentError::Core(error));
        }
        self.drain_core_events();
        while let Some(effect) = self.core.poll_effect() {
            self.execute_effect(effect).await?;
        }
        Ok(())
    }

    pub fn dispatch_datagram(
        &mut self,
        generation: TransportGeneration,
        bytes: Vec<u8>,
    ) -> Result<(), AgentError> {
        self.require_generation(generation)?;
        self.events
            .push_back(AgentEvent::DatagramReceived { generation, bytes });
        Ok(())
    }

    pub fn route_rtp(&mut self, packet: RtpPacket) -> Result<Option<RtpPacket>, MediaError> {
        self.router.route(packet)
    }

    pub fn request_keyframe(&mut self, mid: impl Into<String>, now: MonotonicTime) -> bool {
        self.keyframes.request(mid, now)
    }

    pub fn record_received(&mut self, bytes: usize, now: MonotonicTime) {
        self.bandwidth.record(bytes, now);
    }

    pub fn bandwidth_estimate(&mut self, now: MonotonicTime) -> BandwidthEstimate {
        self.bandwidth.poll(now)
    }

    pub async fn receive_datagram(
        &mut self,
        generation: TransportGeneration,
        buffer: &mut [u8],
    ) -> Result<Option<Vec<u8>>, AgentError> {
        let bytes = match &mut self.transport {
            NativeTransport::None => None,
            NativeTransport::Udp { socket, .. } => {
                let (length, _) = socket
                    .recv_from(buffer)
                    .await
                    .map_err(|error| AgentError::Io(error.to_string()))?;
                debug_assert!(length <= buffer.len());
                let Some(contents) = buffer.get(..length) else {
                    debug_assert!(false, "UDP receive length must fit its buffer");
                    return Err(AgentError::Io("UDP receive exceeded its buffer".to_owned()));
                };
                Some(contents.to_vec())
            }
            NativeTransport::Tcp(session) => session.read_frame().await.map_err(AgentError::Tcp)?,
        };
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        self.record_received(bytes.len(), self.clock.now());
        self.dispatch_datagram(generation, bytes.clone())?;
        Ok(Some(bytes))
    }

    pub fn poll_event(&mut self) -> Option<AgentEvent> {
        self.events.pop_front()
    }

    fn drain_core_events(&mut self) {
        while let Some(event) = self.core.poll_event() {
            self.events.push_back(AgentEvent::Core(event));
        }
    }

    async fn execute_effect(&mut self, effect: CoreEffect) -> Result<(), AgentError> {
        match effect {
            CoreEffect::Connect { generation } => {
                self.active_generation = Some(generation);
                self.events.push_back(AgentEvent::EffectExecuted);
            }
            CoreEffect::Send {
                generation,
                payload,
                ..
            } => {
                self.require_generation(generation)?;
                match &mut self.transport {
                    NativeTransport::None => {}
                    NativeTransport::Udp { socket, peer } => {
                        socket
                            .send_to(&payload, *peer)
                            .await
                            .map_err(|error| AgentError::Io(error.to_string()))?;
                    }
                    NativeTransport::Tcp(session) => {
                        session
                            .write_frame(&payload)
                            .await
                            .map_err(AgentError::Tcp)?;
                    }
                }
                self.events.push_back(AgentEvent::EffectExecuted);
            }
        }
        Ok(())
    }

    fn require_generation(&self, generation: TransportGeneration) -> Result<(), AgentError> {
        let expected = self
            .active_generation
            .unwrap_or_else(|| self.core.generation());
        if generation != expected {
            debug_assert_ne!(generation, expected);
            return Err(AgentError::Core(CoreError::StaleGeneration {
                expected,
                received: generation,
            }));
        }
        Ok(())
    }
}

pub struct AgentRunner {
    driver: AgentDriver,
    commands: mailbox::Receiver<DriverCommand>,
}

impl AgentRunner {
    pub fn new(driver: AgentDriver, commands: mailbox::Receiver<DriverCommand>) -> Self {
        Self { driver, commands }
    }

    pub fn driver(&self) -> &AgentDriver {
        &self.driver
    }

    pub fn driver_mut(&mut self) -> &mut AgentDriver {
        &mut self.driver
    }

    pub async fn run(mut self) -> Result<(), AgentError> {
        loop {
            let command = if let Some(deadline) = self.driver.next_deadline() {
                tokio::select! {
                    command = self.commands.recv() => Some(command),
                    _ = tokio::time::sleep_until(self.driver.clock.at(deadline)) => {
                        self.driver.handle(self.driver.clock.now(), CoreInput::Timer).await?;
                        None
                    }
                }
            } else {
                Some(self.commands.recv().await)
            };
            let Some(command) = command else {
                continue;
            };
            match command {
                Ok(DriverCommand::Input { now, input }) => self.driver.handle(now, input).await?,
                Ok(DriverCommand::Shutdown) | Err(_) => return Ok(()),
            }
        }
    }
}

#[derive(Debug)]
pub enum AgentError {
    Core(CoreError),
    Io(String),
    Tcp(TcpError),
    Media(MediaError),
    Session(SessionError),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core(error) => write!(formatter, "core: {error}"),
            Self::Io(error) => write!(formatter, "I/O: {error}"),
            Self::Tcp(error) => write!(formatter, "TCP: {error}"),
            Self::Media(error) => write!(formatter, "media: {error}"),
            Self::Session(error) => write!(formatter, "session: {error}"),
        }
    }
}

impl std::error::Error for AgentError {}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pulsebeam_agent_core::{CoreEvent, CoreInput};

    #[tokio::test]
    async fn driver_executes_connect_effect_and_rejects_stale_datagrams() {
        let mut driver = AgentDriver::new(CoreConfig::default(), NativeTransport::None);
        driver
            .handle(MonotonicTime::ZERO, CoreInput::Start)
            .await
            .unwrap();
        assert!(matches!(
            driver.poll_event(),
            Some(AgentEvent::Core(CoreEvent::StateChanged { .. }))
        ));
        assert!(matches!(
            driver.poll_event(),
            Some(AgentEvent::EffectExecuted)
        ));
        let result = driver.dispatch_datagram(TransportGeneration::new(0), vec![1]);
        assert!(matches!(
            result,
            Err(AgentError::Core(CoreError::StaleGeneration { .. }))
        ));
    }

    #[tokio::test]
    async fn runner_translates_mailbox_input_to_core_effects() {
        let driver = AgentDriver::new(CoreConfig::default(), NativeTransport::None);
        let (agent, commands) = driver.agent("participant");
        let runner = AgentRunner::new(driver, commands);
        let task = tokio::spawn(runner.run());
        agent.start(MonotonicTime::ZERO).await.unwrap();
        agent.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn udp_dispatch_preserves_generation_and_payload() {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let receiver_addr = receiver.local_addr().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(&[1, 2, 3], receiver_addr).await.unwrap();

        let mut driver = AgentDriver::new(
            CoreConfig::default(),
            NativeTransport::udp(receiver, sender.local_addr().unwrap()),
        );
        driver
            .handle(MonotonicTime::ZERO, CoreInput::Start)
            .await
            .unwrap();
        let mut buffer = [0; 32];
        assert_eq!(
            driver
                .receive_datagram(TransportGeneration::new(1), &mut buffer)
                .await
                .unwrap(),
            Some(vec![1, 2, 3])
        );
        assert!(matches!(
            driver.poll_event(),
            Some(AgentEvent::Core(CoreEvent::StateChanged { .. }))
        ));
        assert!(matches!(
            driver.poll_event(),
            Some(AgentEvent::EffectExecuted)
        ));
        assert_eq!(
            driver.poll_event(),
            Some(AgentEvent::DatagramReceived {
                generation: TransportGeneration::new(1),
                bytes: vec![1, 2, 3]
            })
        );
    }

    #[tokio::test]
    async fn tcp_dispatch_decodes_one_owned_frame() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        crate::tcp::write_frame(&mut server, &[4, 5]).await.unwrap();

        let mut driver = AgentDriver::new(
            CoreConfig::default(),
            NativeTransport::Tcp(TcpSession::new(client)),
        );
        driver
            .handle(MonotonicTime::ZERO, CoreInput::Start)
            .await
            .unwrap();
        let mut buffer = [];
        assert_eq!(
            driver
                .receive_datagram(TransportGeneration::new(1), &mut buffer)
                .await
                .unwrap(),
            Some(vec![4, 5])
        );
        assert!(matches!(
            driver.poll_event(),
            Some(AgentEvent::Core(CoreEvent::StateChanged { .. }))
        ));
        assert!(matches!(
            driver.poll_event(),
            Some(AgentEvent::EffectExecuted)
        ));
        assert_eq!(
            driver.poll_event(),
            Some(AgentEvent::DatagramReceived {
                generation: TransportGeneration::new(1),
                bytes: vec![4, 5]
            })
        );
    }
}
