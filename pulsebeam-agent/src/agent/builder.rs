use crate::TransceiverDirection;
use crate::agent::driver::{AgentDriver, AgentError, DriverInit};
use crate::agent::{Agent, AgentRunner};
use crate::api::{CreateParticipantRequest, HttpApiClient};
use crate::tcp::TcpSession;
use pulsebeam_core::net::UdpSocket;
use pulsebeam_proto::namespace;
use pulsebeam_proto::rtp_extensions;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::channel::{ChannelConfig, Reliability};
use str0m::media::{Direction, MediaAdded, MediaKind, Simulcast, SimulcastLayer};
use str0m::{Candidate, Rtc, net::TcpType};
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub(crate) struct TrackRequest {
    kind: MediaKind,
    direction: TransceiverDirection,
    simulcast_layers: Option<Vec<SimulcastLayer>>,
}

/// Everything needed to construct this participant's `Rtc` from nothing.
///
/// A resume cannot reuse the old `Rtc`: the server rebuilds the participant on a fresh one with a
/// new DTLS certificate, so the client has to arrive with a new certificate too. Keeping the
/// blueprint lets the driver rebuild an equivalent connection without re-running the join.
#[derive(Debug, Clone)]
pub(crate) struct ConnectionBlueprint {
    pub local_ips: Vec<IpAddr>,
    pub port: u16,
    pub negotiate_dependency_descriptor: bool,
    pub tracks: Vec<TrackRequest>,
    pub tcp_active: bool,
    /// Part of the session's shape, so a resume re-establishes it rather than silently
    /// reverting a manual-subscription client to automatic.
    pub manual_sub: bool,
}

pub(crate) struct BuiltConnection {
    pub rtc: Rtc,
    pub signaling_cid: str0m::channel::ChannelId,
    pub medias: Vec<MediaAdded>,
    pub offer: str0m::change::SdpOffer,
    pub pending: str0m::change::SdpPendingOffer,
    pub addr: SocketAddr,
}

/// Build a fresh `Rtc` and its opening offer.
///
/// Media lines are added in blueprint order, so a rebuild reproduces the same mids and the
/// driver's mid-keyed routing stays valid across a resume.
pub(crate) fn build_connection(
    blueprint: &ConnectionBlueprint,
    now: Instant,
) -> Result<BuiltConnection, AgentError> {
    let mut rtc_builder = Rtc::builder()
        .clear_codecs()
        .enable_bwe(Some(Bitrate::kbps(2000)))
        .set_extension(
            rtp_extensions::ABS_CAPTURE_TIME,
            str0m::rtp::Extension::AbsoluteCaptureTime,
        )
        .set_extension(
            rtp_extensions::VIDEO_LAYERS_ALLOCATION,
            str0m::rtp::Extension::with_serializer(
                str0m::rtp::vla::URI,
                str0m::rtp::vla::Serializer,
            ),
        )
        .set_stats_interval(Some(Duration::from_millis(200)));

    if blueprint.negotiate_dependency_descriptor {
        // Per-frame dependency structure, so a scalable source can tell the SFU
        // which frames each decode target needs (temporal/spatial shedding).
        rtc_builder = rtc_builder.set_extension(
            rtp_extensions::DEPENDENCY_DESCRIPTOR,
            str0m::rtp::Extension::with_serializer(
                pulsebeam_core::dd::URI,
                pulsebeam_core::dd::Serializer,
            ),
        );
    }
    // The agent owns packetization/reassembly through the codec-agnostic,
    // DD-driven framing pipeline, so str0m hands us raw RTP in and out.
    rtc_builder = rtc_builder.set_rtp_mode(true);

    let codec_config = rtc_builder.codec_config();
    codec_config.enable_opus(true);
    codec_config.enable_h264(true);

    let mut rtc = rtc_builder.build(now.into());
    let mut candidate_count = 0;
    let mut maybe_addr = None;
    for ip in &blueprint.local_ips {
        let addr = SocketAddr::new(*ip, blueprint.port);
        let Ok(candidate) = Candidate::builder().udp().host(addr).build() else {
            continue;
        };
        rtc.add_local_candidate(candidate);
        maybe_addr = Some(addr);
        candidate_count += 1;
    }

    if blueprint.tcp_active {
        for ip in &blueprint.local_ips {
            let tcp_candidate_addr = SocketAddr::new(*ip, 9);
            if let Ok(c) = Candidate::builder()
                .tcp()
                .host(tcp_candidate_addr)
                .tcptype(TcpType::Active)
                .build()
            {
                rtc.add_local_candidate(c);
                candidate_count += 1;
                if maybe_addr.is_none() {
                    maybe_addr = Some(tcp_candidate_addr);
                }
            }
        }
    }

    if candidate_count == 0 {
        return Err(AgentError::NoCandidates);
    }
    let Some(addr) = maybe_addr else {
        return Err(AgentError::NoCandidates);
    };

    let mut sdp = rtc.sdp_api();
    let signaling_cfg = ChannelConfig {
        label: namespace::Signaling::Reliable.as_str().to_string(),
        ordered: true,
        reliability: Reliability::Reliable,
        negotiated: None,
        protocol: "".to_string(),
    };
    let signaling_cid = sdp.add_channel_with_config(signaling_cfg);

    let mut medias = Vec::new();
    for track in &blueprint.tracks {
        let (dir, simulcast) = match track.direction {
            TransceiverDirection::SendOnly => (
                Direction::SendOnly,
                track.simulcast_layers.clone().map(|layers| Simulcast {
                    send: layers,
                    recv: Vec::new(),
                }),
            ),
            TransceiverDirection::RecvOnly => (
                Direction::RecvOnly,
                track.simulcast_layers.clone().map(|layers| Simulcast {
                    send: Vec::new(),
                    recv: layers,
                }),
            ),
        };
        let mid = sdp.add_media(track.kind, dir, None, None, simulcast.clone());
        medias.push(MediaAdded {
            mid,
            kind: track.kind,
            direction: dir,
            simulcast,
        });
    }

    let (offer, pending) = sdp
        .apply()
        .ok_or_else(|| AgentError::Protocol("SDP apply produced no offer".into()))?;

    Ok(BuiltConnection {
        rtc,
        signaling_cid,
        medias,
        offer,
        pending,
        addr,
    })
}

pub struct AgentBuilder {
    api: HttpApiClient,
    udp_socket: UdpSocket,
    tracks: Vec<TrackRequest>,
    local_ips: Vec<IpAddr>,
    tcp_server_addr: Option<SocketAddr>,
    manual_sub: bool,
    negotiate_dependency_descriptor: bool,
}

/// The facts a join produces, whichever representation carried it.
struct JoinedSession {
    answer: str0m::change::SdpAnswer,
    resource_uri: http::Uri,
    participant_id: String,
    connection_id: Option<String>,
    resume_token: Option<String>,
}

impl AgentBuilder {
    pub fn new(api: HttpApiClient, udp_socket: UdpSocket) -> AgentBuilder {
        Self {
            api,
            udp_socket,
            tracks: Vec::new(),
            local_ips: Vec::new(),
            tcp_server_addr: None,
            manual_sub: false,
            negotiate_dependency_descriptor: true,
        }
    }

    /// Do not negotiate the Dependency Descriptor extension, modelling a
    /// marker/deep-inspection-only peer that predates DD support.
    pub fn without_dependency_descriptor(mut self) -> Self {
        self.negotiate_dependency_descriptor = false;
        self
    }

    pub fn video_upstream_slots(
        mut self,
        capacity: usize,
        simulcast_layers: Option<Vec<SimulcastLayer>>,
    ) -> Self {
        debug_assert!(capacity > 0);
        self.tracks.extend((0..capacity).map(|_| TrackRequest {
            kind: MediaKind::Video,
            direction: TransceiverDirection::SendOnly,
            simulcast_layers: simulcast_layers.clone(),
        }));
        self
    }

    pub fn audio_upstream_slots(mut self, capacity: usize) -> Self {
        debug_assert!(capacity > 0);
        self.tracks.extend((0..capacity).map(|_| TrackRequest {
            kind: MediaKind::Audio,
            direction: TransceiverDirection::SendOnly,
            simulcast_layers: None,
        }));
        self
    }

    pub fn video_downstream_slots(mut self, capacity: usize) -> Self {
        debug_assert!(capacity > 0);
        self.tracks.extend((0..capacity).map(|_| TrackRequest {
            kind: MediaKind::Video,
            direction: TransceiverDirection::RecvOnly,
            simulcast_layers: None,
        }));
        self
    }

    pub fn audio_downstream_slots(mut self, capacity: usize) -> Self {
        debug_assert!(capacity > 0);
        self.tracks.extend((0..capacity).map(|_| TrackRequest {
            kind: MediaKind::Audio,
            direction: TransceiverDirection::RecvOnly,
            simulcast_layers: None,
        }));
        self
    }

    pub fn with_local_ip(mut self, ip: IpAddr) -> Self {
        self.local_ips.push(ip);
        self
    }

    pub fn with_tcp_server_addr(mut self, addr: SocketAddr) -> Self {
        self.tcp_server_addr = Some(addr);
        self
    }

    /// Keep downstream slots unassigned until the application sends explicit subscriptions.
    pub fn manual_subscriptions(mut self) -> Self {
        self.manual_sub = true;
        self
    }

    pub async fn connect(self, room_id: &str) -> Result<Agent, AgentError> {
        let (agent, runner) = self.connect_unmanaged(room_id).await?;
        tokio::spawn(async move {
            if let Err(error) = runner.run().await {
                tracing::error!(?error, "agent runner stopped");
            }
        });
        Ok(agent)
    }

    pub async fn connect_unmanaged(
        mut self,
        room_id: &str,
    ) -> Result<(Agent, AgentRunner), AgentError> {
        let port = self.udp_socket.local_addr()?.port();

        if self.local_ips.is_empty() {
            self.local_ips.extend(
                if_addrs::get_if_addrs()?
                    .into_iter()
                    .filter(|i| !i.is_loopback())
                    .map(|i| i.ip()),
            )
        }

        let blueprint = ConnectionBlueprint {
            local_ips: self.local_ips.clone(),
            port,
            negotiate_dependency_descriptor: self.negotiate_dependency_descriptor,
            tracks: self.tracks.clone(),
            tcp_active: self.tcp_server_addr.is_some(),
            manual_sub: self.manual_sub,
        };

        // Established before building the Rtc so the TCP candidate it advertises is backed by a
        // real stream.
        let mut tcp_stream: Option<pulsebeam_core::net::TcpStream> = None;
        let mut tcp_local_addr: Option<SocketAddr> = None;
        let mut tcp_server_addr: Option<SocketAddr> = None;
        if let Some(server_tcp) = self.tcp_server_addr {
            match pulsebeam_core::net::TcpStream::connect(server_tcp).await {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    tcp_local_addr = stream.local_addr().ok();
                    tcp_server_addr = Some(server_tcp);
                    tcp_stream = Some(stream);
                }
                Err(e) => return Err(AgentError::Io(e)),
            }
        }

        let built = build_connection(&blueprint, Instant::now())?;
        let BuiltConnection {
            mut rtc,
            signaling_cid,
            medias,
            offer,
            pending,
            addr,
        } = built;

        // The representation is chosen once, on the client; both produce the same session facts.
        let session = match self.api.protocol() {
            crate::api::Protocol::Json => {
                let resp = self.api.join_json(room_id, offer, self.manual_sub).await?;
                let answer = str0m::change::SdpAnswer::from_sdp_string(&resp.sdp)
                    .map_err(|e| AgentError::Api(crate::api::ApiError::SdpError(e)))?;
                JoinedSession {
                    answer,
                    resource_uri: resp.resource.parse().map_err(|_| {
                        AgentError::Api(crate::api::ApiError::Protocol(
                            "server returned an unusable resource url".to_string(),
                        ))
                    })?,
                    participant_id: resp.participant_id,
                    connection_id: Some(resp.connection_id),
                    resume_token: Some(resp.resume_token),
                }
            }
            crate::api::Protocol::Sdp => {
                let resp = self
                    .api
                    .create_participant(CreateParticipantRequest {
                        room_id: room_id.to_string(),
                        offer,
                        manual_sub: self.manual_sub,
                    })
                    .await?;
                JoinedSession {
                    answer: resp.answer,
                    resource_uri: resp.resource_uri,
                    participant_id: resp.participant_id,
                    connection_id: resp.connection_id,
                    resume_token: None,
                }
            }
        };

        rtc.sdp_api()
            .accept_answer(pending, session.answer)
            .map_err(AgentError::Rtc)?;

        let init = DriverInit {
            api: self.api,
            addr,
            rtc,
            socket: self.udp_socket,
            tcp: match tcp_stream {
                Some(s) => TcpSession::new(s, tcp_local_addr, tcp_server_addr.unwrap()),
                None => TcpSession::inactive(),
            },
            signaling_cid,
            blueprint,
            resource_uri: session.resource_uri,
            room_id: room_id.to_string(),
            participant_id: session.participant_id,
            connection_id: session.connection_id,
            resume_token: session.resume_token,
            medias,
        };

        Ok(AgentRunner::new(AgentDriver::new(init)))
    }
}
