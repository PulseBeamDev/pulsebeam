use crate::TransceiverDirection;
use crate::agent::driver::{AgentDriver, AgentError, DriverInit, MediaTemplate, RtcTemplate};
use crate::agent::{Agent, AgentRunner};
use crate::api::{CreateParticipantRequest, HttpApiClient};
use crate::tcp::TcpSession;
use pulsebeam_core::net::UdpSocket;
use pulsebeam_proto::rtp_extensions;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use str0m::bwe::Bitrate;
use str0m::media::{Direction, MediaKind, Simulcast, SimulcastLayer};
use str0m::{Candidate, Rtc, net::TcpType};

#[derive(Debug, Clone)]
struct TrackRequest {
    kind: MediaKind,
    direction: TransceiverDirection,
    simulcast_layers: Option<Vec<SimulcastLayer>>,
}

pub struct AgentBuilder {
    api: HttpApiClient,
    udp_socket: UdpSocket,
    tracks: Vec<TrackRequest>,
    local_ips: Vec<IpAddr>,
    tcp_server_addr: Option<SocketAddr>,
    manual_sub: bool,
    negotiate_dependency_descriptor: bool,
    initial_send_bitrate: Bitrate,
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
            initial_send_bitrate: Bitrate::kbps(2000),
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

    pub fn with_initial_send_bitrate_bps(mut self, bitrate_bps: u64) -> Self {
        debug_assert!(bitrate_bps > 0);
        self.initial_send_bitrate = Bitrate::bps(bitrate_bps);
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
            );
        }

        let mut rtc_builder = Rtc::builder()
            .clear_codecs()
            .enable_bwe(Some(self.initial_send_bitrate))
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

        if self.negotiate_dependency_descriptor {
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
        //
        // let baseline_levels = [0x34];
        // let mut pt = 96;
        //
        // for level in &baseline_levels {
        //     codec_config.add_h264(pt.into(), Some((pt + 1).into()), true, 0x42e000 | level);
        //     pt += 2;
        // }

        let rtc_config = rtc_builder.clone();
        let mut candidate_count = 0usize;
        let mut maybe_addr = None;
        let mut candidates = Vec::new();
        for ip in &self.local_ips {
            let addr = SocketAddr::new(*ip, port);
            let Ok(candidate) = Candidate::builder().udp().host(addr).build() else {
                continue;
            };
            candidates.push(candidate);
            maybe_addr = Some(addr);
            candidate_count = candidate_count.saturating_add(1);
        }

        let mut tcp_stream: Option<pulsebeam_core::net::TcpStream> = None;
        let mut tcp_local_addr: Option<SocketAddr> = None;
        let mut tcp_server_addr: Option<SocketAddr> = None;
        if let Some(server_tcp) = self.tcp_server_addr {
            match pulsebeam_core::net::TcpStream::connect(server_tcp).await {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    let local = stream.local_addr().ok();
                    tcp_local_addr = local;
                    tcp_server_addr = Some(server_tcp);
                    tcp_stream = Some(stream);

                    for ip in &self.local_ips {
                        let tcp_candidate_addr = SocketAddr::new(*ip, 9);
                        if let Ok(c) = Candidate::builder()
                            .tcp()
                            .host(tcp_candidate_addr)
                            .tcptype(TcpType::Active)
                            .build()
                        {
                            candidates.push(c);
                            candidate_count = candidate_count.saturating_add(1);
                            if maybe_addr.is_none() {
                                maybe_addr = Some(tcp_candidate_addr);
                            }
                        }
                    }
                }
                Err(e) => {
                    return Err(AgentError::Io(e));
                }
            }
        }

        if candidate_count == 0 {
            return Err(AgentError::NoCandidates);
        }

        let Some(addr) = maybe_addr else {
            return Err(AgentError::NoCandidates);
        };

        let media_templates = self
            .tracks
            .iter()
            .map(|track| {
                let (direction, simulcast) = match track.direction {
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
                MediaTemplate {
                    kind: track.kind,
                    direction,
                    simulcast,
                }
            })
            .collect();
        let rtc_template = RtcTemplate::new(rtc_config, candidates, media_templates);
        let (mut rtc, signaling_cid, medias, offer, pending) = rtc_template.build()?;

        let resp = self
            .api
            .create_participant(CreateParticipantRequest {
                room_id: room_id.to_string(),
                offer,
                manual_sub: self.manual_sub,
            })
            .await?;

        rtc.sdp_api()
            .accept_answer(pending, resp.answer)
            .map_err(AgentError::Rtc)?;

        let init = DriverInit {
            api: self.api,
            addr,
            rtc,
            socket: self.udp_socket,
            tcp: match (tcp_stream, tcp_server_addr) {
                (Some(s), Some(addr)) => TcpSession::new(s, tcp_local_addr, addr),
                // A stream without a server address cannot be addressed, so it
                // is no more usable than having no stream at all.
                _ => TcpSession::inactive(),
            },
            signaling_cid,
            resource_uri: resp.resource_uri,
            etag: resp.etag,
            #[cfg(feature = "sim")]
            room_id: room_id.to_string(),
            participant_id: resp.participant_id,
            medias,
            rtc_template,
        };

        Ok(AgentRunner::new(AgentDriver::new(init)))
    }
}
