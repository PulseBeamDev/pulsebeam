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
        }
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

        let mut rtc = rtc_builder.build(Instant::now().into());
        let mut candidate_count = 0;
        let mut maybe_addr = None;
        for ip in &self.local_ips {
            let addr = SocketAddr::new(*ip, port);
            let candidate = match Candidate::builder().udp().host(addr).build() {
                Ok(candidate) => candidate,
                Err(_) => {
                    continue;
                }
            };
            rtc.add_local_candidate(candidate);
            maybe_addr = Some(addr);
            candidate_count += 1;
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
                            rtc.add_local_candidate(c);
                            candidate_count += 1;
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
        for track in self.tracks.clone() {
            let (dir, simulcast) = match track.direction {
                TransceiverDirection::SendOnly => (
                    Direction::SendOnly,
                    track.simulcast_layers.map(|layers| Simulcast {
                        send: layers,
                        recv: Vec::new(),
                    }),
                ),
                TransceiverDirection::RecvOnly => (
                    Direction::RecvOnly,
                    track.simulcast_layers.map(|layers| Simulcast {
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
            tcp: match tcp_stream {
                Some(s) => TcpSession::new(s, tcp_local_addr, tcp_server_addr.unwrap()),
                None => TcpSession::inactive(),
            },
            signaling_cid,
            resource_uri: resp.resource_uri,
            room_id: room_id.to_string(),
            participant_id: resp.participant_id,
            medias,
        };

        Ok(AgentRunner::new(AgentDriver::new(init)))
    }
}
