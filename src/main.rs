use anyhow::Result;
use bytes::Bytes;
use clap::Parser;
use futures::StreamExt;
use parking_lot::Mutex; // Using parking_lot for Mutex in tests potentially
use serde::Deserialize;
use std::{
    collections::{HashMap, VecDeque},
    fs,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use str0m::{
    format::CodecConfig,
    media::{Direction, MediaKind},
    net::{Protocol, Receive, Transmit},
    rtp::{ExtensionValues, RtpHeader, Ssrc},
    rtcp::{Nack, Rtcp},
    Candidate, Event, IceConnectionState, Input, KeyframeRequestKind, MediaTime, Output, Rtc,
    CandidateKind, // Required for Candidate::host
    RtcError, // Import RtcError explicitly
};
use thiserror::Error;
use tokio::{net::UdpSocket, sync::mpsc, time::timeout};
use tracing::{debug, error, info, trace, warn, Level};
use tracing_subscriber::FmtSubscriber;
use uuid::Uuid;
use rstest::*; // For testing

// --- Configuration ---
#[derive(Debug, Deserialize, Clone)]
pub struct SfuConfig {
    #[serde(default = "default_rtp_packet_buffer_size")]
    rtp_packet_buffer_size: usize,
    #[serde(with = "humantime_serde", default = "default_peer_timeout_duration")]
    peer_timeout_duration: Duration,
    #[serde(with = "humantime_serde", default = "default_task_interval")]
    task_interval: Duration,
    #[serde(default = "default_udp_recv_buf_size")]
    udp_recv_buf_size: usize,
    #[serde(default)] // Add defaults for new config items
    ice_failed_timeout_duration: Option<Duration>, // Optional: Timeout for stuck ICE states
}

// Default functions for serde
fn default_rtp_packet_buffer_size() -> usize { 256 }
fn default_peer_timeout_duration() -> Duration { Duration::from_secs(60) }
fn default_task_interval() -> Duration { Duration::from_millis(10) }
fn default_udp_recv_buf_size() -> usize { 2000 }


impl Default for SfuConfig {
    fn default() -> Self {
        SfuConfig {
            rtp_packet_buffer_size: default_rtp_packet_buffer_size(),
            peer_timeout_duration: default_peer_timeout_duration(),
            task_interval: default_task_interval(),
            udp_recv_buf_size: default_udp_recv_buf_size(),
            ice_failed_timeout_duration: Some(Duration::from_secs(20)), // Example default
        }
    }
}

// --- Metrics ---
#[derive(Debug, Default)]
pub struct SfuMetrics {
    pub peers_added: AtomicUsize,
    pub peers_removed: AtomicUsize,
    pub groups_active: AtomicUsize, // Gauge derived from map size
    pub rtp_received: AtomicUsize,
    pub rtp_forwarded_attempts: AtomicUsize, // Attempts to forward
    pub rtp_forwarded_success: AtomicUsize, // Successful write_rtp calls
    pub rtp_bytes_in: AtomicUsize,
    pub rtp_bytes_forwarded: AtomicUsize,
    pub nacks_received: AtomicUsize,
    pub nacks_processed: AtomicUsize, // Found buffer for SSRC
    pub nack_packets_requested: AtomicUsize, // Sum of sequence numbers in NACKs
    pub nack_packets_found_in_buffer: AtomicUsize,
    pub nack_packets_retransmitted: AtomicUsize,
    pub keyframes_requested_in: AtomicUsize, // PLI/FIR received from client
    pub keyframes_requested_out: AtomicUsize, // PLI/FIR forwarded to origin peer
    pub udp_packets_in: AtomicUsize,
    pub udp_packets_out: AtomicUsize,
    pub udp_bytes_in: AtomicUsize,
    pub udp_bytes_out: AtomicUsize,
}

impl SfuMetrics {
     // Method to easily get gauge values (requires access to Sfu state, so maybe call from outside)
    // Or pass maps lengths periodically if SFU owns metrics exclusively.
    // For simplicity, we'll just have the counters here.
    pub fn log_summary(&self) {
         info!(
            peers_added = self.peers_added.load(Ordering::Relaxed),
            peers_removed = self.peers_removed.load(Ordering::Relaxed),
            rtp_received = self.rtp_received.load(Ordering::Relaxed),
            rtp_forwarded_attempts = self.rtp_forwarded_attempts.load(Ordering::Relaxed),
            rtp_bytes_in = self.rtp_bytes_in.load(Ordering::Relaxed),
            nacks_received = self.nacks_received.load(Ordering::Relaxed),
            nack_packets_retransmitted = self.nack_packets_retransmitted.load(Ordering::Relaxed),
            udp_packets_in = self.udp_packets_in.load(Ordering::Relaxed),
            udp_packets_out = self.udp_packets_out.load(Ordering::Relaxed),
            udp_bytes_in = self.udp_bytes_in.load(Ordering::Relaxed),
            udp_bytes_out = self.udp_bytes_out.load(Ordering::Relaxed),
            "SFU Metrics Summary"
        );
    }
}


// --- Types (Error, Command, Event, Core Structures) ---
#[derive(Error, Debug)]
pub enum SfuError {
    #[error("Strum error: {0}")]
    Strum(#[from] str0m::Error),
    #[error("Peer {0} not found in group {1}")]
    PeerNotFound(PeerId, GroupId),
    #[error("Group {0} not found")]
    GroupNotFound(GroupId),
    #[error("Peer {0} already exists in group {1}")]
    PeerAlreadyExists(PeerId, GroupId),
    #[error("Failed to parse candidate: {0}")]
    CandidateParseError(String),
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Send error on internal channel")]
    SendError,
    #[error("Unknown UDP source address: {0}")]
    UnknownUdpSource(SocketAddr),
    #[error("Inconsistent state: {0}")]
    InconsistentState(String),
    #[error("Configuration error: {0}")]
    Config(String), // Added config error
}

#[derive(Debug)]
pub enum SfuCommand {
    AddPeer {
        group_id: GroupId,
        peer_id: PeerId,
        offer: String,
    },
    HandleAnswer {
        group_id: GroupId,
        peer_id: PeerId,
        answer: String,
    },
    HandleCandidate {
        group_id: GroupId,
        peer_id: PeerId,
        candidate: String,
    },
    RemovePeer {
        peer_id: PeerId,
    },
    HandleUdpPacket {
        data: Bytes,
        remote_addr: SocketAddr,
        local_addr: SocketAddr, // Added local address
    },
    Tick,
    GetMetrics, // Command to request metrics snapshot
}

#[derive(Debug, Clone)]
pub enum SfuEvent {
    SdpAnswerReady {
        peer_id: PeerId,
        answer: String,
    },
    IceCandidateReady {
        peer_id: PeerId,
        candidate: String,
    },
    IceConnectionStateChange {
        peer_id: PeerId,
        state: IceConnectionState,
    },
    Connected {
        peer_id: PeerId,
    },
    PeerRemoved {
        peer_id: PeerId,
        group_id: GroupId,
        reason: String, // Added reason for removal
    },
    TransmitPacket(Transmit),
    MetricsSnapshot(Arc<SfuMetrics>), // Send metrics back
    Error(String),
}

#[derive(Debug, Clone)]
struct BufferedPacket {
    seq_no: u16,
    pt: u8,
    marker: bool,
    timestamp: u32,
    time: MediaTime,
    ssrc: Ssrc,
    payload: Bytes,
}

pub struct Sfu {
    config: Arc<SfuConfig>, // Use config
    metrics: Arc<SfuMetrics>, // Use metrics
    groups: HashMap<GroupId, Group>,
    peers_by_addr: HashMap<SocketAddr, PeerId>,
    peer_id_to_group: HashMap<PeerId, GroupId>,
    event_tx: mpsc::Sender<SfuEvent>,
}

struct Group {
    id: GroupId,
    peers: HashMap<PeerId, PeerState>,
    media_ssrc_origins: HashMap<Ssrc, PeerId>,
}

struct PeerState {
    id: PeerId,
    rtc: Rtc,
    last_activity: Instant,
    connected_time: Option<Instant>, // Track when connection established
    ice_state: IceConnectionState, // Keep track of state for timeouts
    nack_buffer: HashMap<Ssrc, VecDeque<BufferedPacket>>,
    remote_addr: Option<SocketAddr>,
    group_id: GroupId,
}

impl Sfu {
    pub fn new(config: Arc<SfuConfig>, metrics: Arc<SfuMetrics>, event_tx: mpsc::Sender<SfuEvent>) -> Self {
        Sfu {
            config,
            metrics,
            groups: HashMap::new(),
            peers_by_addr: HashMap::new(),
            peer_id_to_group: HashMap::new(),
            event_tx,
        }
    }

    pub fn handle_command(&mut self, cmd: SfuCommand) {
        let result = match cmd {
            SfuCommand::AddPeer { group_id, peer_id, offer } => {
                self.add_peer(group_id, peer_id, offer)
            }
            SfuCommand::HandleAnswer { group_id, peer_id, answer } => {
                self.handle_peer_answer(&group_id, &peer_id, answer)
            }
            SfuCommand::HandleCandidate { group_id, peer_id, candidate } => {
                self.handle_peer_candidate(&group_id, &peer_id, candidate)
            }
            SfuCommand::RemovePeer { peer_id } => {
                self.remove_peer(&peer_id, "command".to_string());
                Ok(())
            }
            SfuCommand::HandleUdpPacket { data, remote_addr, local_addr } => {
                self.metrics.udp_packets_in.fetch_add(1, Ordering::Relaxed);
                self.metrics.udp_bytes_in.fetch_add(data.len(), Ordering::Relaxed);
                self.handle_udp_input(data, remote_addr, local_addr)
            }
            SfuCommand::Tick => {
                self.poll_and_forward();
                Ok(())
            }
            SfuCommand::GetMetrics => {
                // Send a clone of the Arc, receiver can access atomics
                self.send_event(SfuEvent::MetricsSnapshot(self.metrics.clone()))
            }
        };

        if let Err(e) = result {
            error!("Error handling command: {}", e);
            let _ = self.send_event(SfuEvent::Error(e.to_string()));
        }
    }

    // --- Internal Logic ---

    fn add_peer(&mut self, group_id: GroupId, peer_id: PeerId, offer: String) -> Result<(), SfuError> {
        info!(%peer_id, %group_id, "Adding new peer");
        let group = self.groups.entry(group_id.clone()).or_insert_with(|| Group {
            id: group_id.clone(),
            peers: HashMap::new(),
            media_ssrc_origins: HashMap::new(),
        });

        if group.peers.contains_key(&peer_id) {
            return Err(SfuError::PeerAlreadyExists(peer_id, group_id));
        }

        // Security: Consider limiting complexity/size of incoming SDP offer
        if offer.len() > 10000 { // Example limit
            warn!(%peer_id, "Rejecting overly large SDP offer");
            return Err(SfuError::Config("SDP offer too large".into()));
        }

        let mut rtc = Rtc::builder()
            .set_rtp_mode(true)
            .set_ice_lite(false)
            .set_codec_preferences(Some(vec![ /* Defined earlier - sensible defaults */
                 CodecConfig::vp9_default(), CodecConfig::vp8_default(),
                 CodecConfig::h264_constrained_baseline_default(), CodecConfig::opus_default(),
                 CodecConfig::ulpfec_config(), CodecConfig::flexfec_config(), CodecConfig::red_opus_config(),
            ]))
            .build();

        rtc.add_media(MediaKind::Audio, Direction::SendRecv, None);
        rtc.add_media(MediaKind::Video, Direction::SendRecv, None);

        let answer = rtc.handle_offer(&offer)?;
        info!(%peer_id, "Handled initial offer, generated answer");

        let peer_state = PeerState {
            id: peer_id,
            rtc,
            last_activity: Instant::now(),
            connected_time: None,
            ice_state: IceConnectionState::New, // Initial state
            nack_buffer: HashMap::new(),
            remote_addr: None,
            group_id: group_id.clone(),
        };

        group.peers.insert(peer_id, peer_state);
        self.peer_id_to_group.insert(peer_id, group_id.clone());
        self.metrics.peers_added.fetch_add(1, Ordering::Relaxed);

        self.send_event(SfuEvent::SdpAnswerReady { peer_id, answer: answer.to_sdp() })?;
        Ok(())
    }

    fn handle_peer_answer(&mut self, group_id: &GroupId, peer_id: &PeerId, answer: String) -> Result<(), SfuError> {
        // Security: Limit answer size
        if answer.len() > 10000 {
            return Err(SfuError::Config("SDP answer too large".into()));
        }
        let peer = self.get_peer_mut(group_id, peer_id)?;
        peer.rtc.handle_answer(&answer)?;
        info!(%peer_id, "Handled SDP Answer");
        peer.last_activity = Instant::now();
        Ok(())
    }


    fn handle_peer_candidate(&mut self, group_id: &GroupId, peer_id: &PeerId, candidate_str: String) -> Result<(), SfuError> {
        // Security: Limit candidate string length
        if candidate_str.len() > 512 {
            return Err(SfuError::Config("ICE candidate string too long".into()));
        }
        let peer = self.get_peer_mut(group_id, peer_id)?;
        match Candidate::from_sdp(&candidate_str) {
            Ok(candidate) => {
                info!(%peer_id, candidate = %candidate.to_sdp(), "Adding remote ICE candidate");
                // Security: Could potentially rate limit candidate additions per peer
                peer.rtc.add_remote_candidate(candidate);
                peer.last_activity = Instant::now();
                Ok(())
            }
            Err(e) => Err(SfuError::CandidateParseError(e.to_string())),
        }
    }

    fn remove_peer(&mut self, peer_id: &PeerId, reason: String) {
        info!(%peer_id, %reason, "Removing peer");
        if let Some(group_id) = self.peer_id_to_group.remove(peer_id) {
            let mut group_empty = false;
            if let Some(group) = self.groups.get_mut(&group_id) {
                if let Some(removed_peer) = group.peers.remove(peer_id) {
                    if let Some(addr) = removed_peer.remote_addr {
                        self.peers_by_addr.remove(&addr);
                    }
                    group.media_ssrc_origins.retain(|_ssrc, origin_peer_id| origin_peer_id != peer_id);
                    self.metrics.peers_removed.fetch_add(1, Ordering::Relaxed);
                    let _ = self.send_event(SfuEvent::PeerRemoved { peer_id: *peer_id, group_id: group_id.clone(), reason });
                }
                group_empty = group.peers.is_empty();
            }
            if group_empty {
                info!(%group_id, "Group is empty, removing group");
                self.groups.remove(&group_id);
            }
        } else {
            warn!(%peer_id, "Attempted to remove peer not found in any group");
        }
    }

    fn get_peer_mut(&mut self, group_id: &GroupId, peer_id: &PeerId) -> Result<&mut PeerState, SfuError> {
        self.groups
            .get_mut(group_id)
            .ok_or_else(|| SfuError::GroupNotFound(group_id.clone()))?
            .peers
            .get_mut(peer_id)
            .ok_or_else(|| SfuError::PeerNotFound(*peer_id, group_id.clone()))
    }

    fn handle_udp_input(&mut self, data: Bytes, remote_addr: SocketAddr, local_addr: SocketAddr) -> Result<(), SfuError> {
        let peer_id = match self.peers_by_addr.get(&remote_addr) {
            Some(id) => *id,
            None => {
                // Still relying on external ICE Trickle/mapping for now.
                return Err(SfuError::UnknownUdpSource(remote_addr));
            }
        };

        let group_id = self.peer_id_to_group.get(&peer_id)
            .ok_or_else(|| SfuError::InconsistentState(format!("Peer {} found by addr but not in group map", peer_id)))?
            .clone();

        let peer = self.get_peer_mut(&group_id, &peer_id)?;
        trace!(peer = %peer_id, %remote_addr, len = data.len(), "Handling UDP input for known peer");
        peer.last_activity = Instant::now();

        let input = Input::Receive(
            Instant::now(),
            Receive { proto: Protocol::Udp, source: remote_addr, destination: local_addr, contents: data },
        );

        // Important: Use handle_input_no_timeout to avoid blocking if the timeout logic is handled in the main poll loop
        peer.rtc.handle_input(input)?;

        Ok(())
    }

    fn poll_and_forward(&mut self) {
        let mut peers_to_remove = Vec::<(PeerId, String)>::new(); // Store (peer_id, reason)
        let mut new_addr_mappings = Vec::new();
        let mut transmits = Vec::new();

        let group_ids: Vec<GroupId> = self.groups.keys().cloned().collect();

        for group_id in group_ids {
            if !self.groups.contains_key(&group_id) { continue; }

            let peer_ids: Vec<PeerId> = self.groups[&group_id].peers.keys().cloned().collect();

            for peer_id in peer_ids {
                if !self.groups.contains_key(&group_id) || !self.groups[&group_id].peers.contains_key(&peer_id) {
                    continue;
                }

                 // --- Resource Management / Timeout Check ---
                let mut should_remove = None; // Store reason if removal needed
                if let Some(peer) = self.groups[&group_id].peers.get(&peer_id) {
                    let now = Instant::now();
                    if peer.last_activity.elapsed() > self.config.peer_timeout_duration {
                        warn!(%peer_id, %group_id, "Peer timed out due to inactivity");
                        should_remove = Some("inactivity_timeout".to_string());
                    } else if let Some(fail_timeout) = self.config.ice_failed_timeout_duration {
                         // Check for peers stuck in non-connected states for too long
                        match peer.ice_state {
                            IceConnectionState::Disconnected | IceConnectionState::Failed | IceConnectionState::Closed => {
                                 // If we have a connection time, check how long it's been disconnected
                                 if let Some(conn_time) = peer.connected_time {
                                     if conn_time.elapsed() > fail_timeout { // Re-use timeout for post-connection failure
                                        warn!(%peer_id, %group_id, state=?peer.ice_state, "Peer removed due to prolonged failed state after connection");
                                        should_remove = Some(format!("ice_failed_timeout ({:?})", peer.ice_state));
                                     }
                                 } else if peer.last_activity.elapsed() > fail_timeout { // If never connected, use last_activity base
                                     warn!(%peer_id, %group_id, state=?peer.ice_state, "Peer removed due to prolonged failed state before connection");
                                     should_remove = Some(format!("ice_init_failed_timeout ({:?})", peer.ice_state));
                                 }
                            }
                            IceConnectionState::Checking | IceConnectionState::Connecting | IceConnectionState::New => {
                                if peer.connected_time.is_none() && peer.last_activity.elapsed() > fail_timeout {
                                     warn!(%peer_id, %group_id, state=?peer.ice_state, "Peer removed due to prolonged connecting state");
                                     should_remove = Some(format!("ice_connecting_timeout ({:?})", peer.ice_state));
                                }
                            }
                            _ => {} // Connected, Completed are fine
                        }
                    }
                } else { continue; }

                if let Some(reason) = should_remove {
                    peers_to_remove.push((peer_id, reason));
                    continue; // Don't process events for peer marked for removal
                }

                // --- Poll str0m for Outputs ---
                let mut peer_events = Vec::new();
                let rtc = &mut self.groups.get_mut(&group_id).unwrap().peers.get_mut(&peer_id).unwrap().rtc;

                if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                    if matches!(e, RtcError::NotReady) { // Ignore NotReady errors during timeout poll
                        trace!(%peer_id, "Ignoring NotReady during timeout poll");
                    } else {
                         error!(peer = %peer_id, "Error handling str0m timeout input: {}", e);
                         peers_to_remove.push((peer_id, "timeout_input_error".to_string()));
                         continue;
                    }
                }

                loop {
                    match rtc.poll_output() {
                        Ok(output) => match output {
                            Output::Transmit(t) => transmits.push(t),
                            Output::Timeout(_) => {} // Handled by Input::Timeout
                            Output::Event(evt) => peer_events.push(evt),
                        },
                        Err(str0m::Error::NoEvents) => break,
                        Err(e) => {
                            error!(peer = %peer_id, "Error polling str0m output: {}", e);
                            peers_to_remove.push((peer_id, "poll_output_error".to_string()));
                            break;
                        }
                    }
                }

                // --- Process Collected Events ---
                for event in peer_events {
                     // Re-borrow self mutably inside event processing if needed by handle_str0m_event
                    let event_result = self.handle_str0m_event(&group_id, &peer_id, event);
                    if let Err(e) = event_result {
                        error!("Error handling str0m event for peer {}: {}", peer_id, e);
                         if matches!(e, SfuError::Strum(_)) {
                           peers_to_remove.push((peer_id, "str0m_event_error".to_string()));
                        }
                        let _ = self.send_event(SfuEvent::Error(e.to_string()));
                    }
                }

                // --- Update ICE Address Mapping ---
                 if let Some(peer) = self.groups.get_mut(&group_id).unwrap().peers.get_mut(&peer_id) {
                    if let Some(remote_c) = peer.rtc.direct_api().remote_candidate() {
                         if peer.remote_addr != Some(remote_c.addr) {
                            info!(peer=%peer_id, addr=%remote_c.addr, "Updating mapped address");
                            if let Some(old_addr) = peer.remote_addr{ self.peers_by_addr.remove(&old_addr); }
                            peer.remote_addr = Some(remote_c.addr);
                            new_addr_mappings.push((remote_c.addr, peer_id));
                         }
                     }
                }

            } // End peer loop
        } // End group loop

        // Apply changes after iteration
        for (peer_id, reason) in peers_to_remove {
            self.remove_peer(&peer_id, reason);
        }
        for (addr, peer_id) in new_addr_mappings {
            if self.peer_id_to_group.contains_key(&peer_id) {
                self.peers_by_addr.insert(addr, peer_id);
            }
        }

        // Emit transmit events
        for t in transmits {
             self.metrics.udp_packets_out.fetch_add(1, Ordering::Relaxed);
             self.metrics.udp_bytes_out.fetch_add(t.contents.len(), Ordering::Relaxed);
            let _ = self.send_event(SfuEvent::TransmitPacket(t));
        }
    }

    fn handle_str0m_event(&mut self, group_id: &GroupId, peer_id: &PeerId, event: Event) -> Result<(), SfuError> {
        match event {
            Event::MediaData(media) => {
                trace!(peer = %peer_id, ssrc = %media.ssrc, pt = %media.pt, seq = %media.seq_no, len = media.payload.len(), "RTP received");
                self.metrics.rtp_received.fetch_add(1, Ordering::Relaxed);
                self.metrics.rtp_bytes_in.fetch_add(media.payload.len(), Ordering::Relaxed);

                let group = self.groups.get_mut(group_id).ok_or_else(|| SfuError::GroupNotFound(group_id.clone()))?;
                if !group.media_ssrc_origins.contains_key(&media.ssrc) {
                    group.media_ssrc_origins.insert(media.ssrc, *peer_id);
                }

                let buffered_packet = BufferedPacket { /* ... created as before ... */
                    seq_no: media.seq_no, pt: media.pt, marker: media.marker, timestamp: media.timestamp,
                    time: media.time, ssrc: media.ssrc, payload: media.payload.clone(),
                };
                let origin_peer_id = *peer_id;
                let recipient_ids: Vec<PeerId> = group.peers.keys().cloned().filter(|p| *p != origin_peer_id).collect();

                for recipient_peer_id in recipient_ids {
                    self.metrics.rtp_forwarded_attempts.fetch_add(1, Ordering::Relaxed);
                     // Re-borrow mutably group inside loop
                    if let Some(recipient_peer) = self.groups.get_mut(group_id).unwrap().peers.get_mut(&recipient_peer_id) {
                        // --- NACK Buffering ---
                        let buffer = recipient_peer.nack_buffer.entry(media.ssrc).or_insert_with(VecDeque::new);
                        buffer.push_back(buffered_packet.clone());
                        if buffer.len() > self.config.rtp_packet_buffer_size { // Use config
                            buffer.pop_front();
                        }
                        // --- Forwarding ---
                        match recipient_peer.rtc.write_rtp(
                            media.ssrc, media.pt, media.seq_no, media.marker, media.timestamp,
                            media.time, media.payload.clone(), ExtensionValues::default(),
                        ) {
                            Ok(_) => {
                                self.metrics.rtp_forwarded_success.fetch_add(1, Ordering::Relaxed);
                                self.metrics.rtp_bytes_forwarded.fetch_add(media.payload.len(), Ordering::Relaxed);
                                trace!(sender = %origin_peer_id, recipient = %recipient_peer_id, ssrc = %media.ssrc, seq=%media.seq_no, "RTP forwarded");
                            }
                            Err(e) => {
                                error!(sender = %origin_peer_id, recipient = %recipient_peer_id, ssrc = %media.ssrc, "Failed to write RTP to peer: {}", e);
                                // Don't error out the whole handler, just log failure for this recipient
                            }
                        }
                    }
                }
            }
            Event::RtcpData(rtcp_data) => {
                trace!(peer = %peer_id, ssrc = %rtcp_data.ssrc, len = rtcp_data.payload.len(), "RTCP received");
                 // Security: Limit RTCP packet size?
                 if rtcp_data.payload.len() > 1500 {
                    warn!(%peer_id, "Ignoring oversized RTCP packet");
                    return Ok(());
                 }
                match Rtcp::read_packet(&rtcp_data.payload) {
                    Ok(Rtcp::TransportLayerFeedback(feedback)) if feedback.is_nack() => {
                        let nack = feedback.nack().unwrap();
                        self.metrics.nacks_received.fetch_add(1, Ordering::Relaxed);
                        let missing_media_ssrc = nack.ssrc();
                        let requesting_peer_id = *peer_id;
                        let nack_count = nack.iter().count();
                        info!(peer = %requesting_peer_id, media_ssrc = %missing_media_ssrc, count = nack_count, "Received NACK");
                        self.metrics.nack_packets_requested.fetch_add(nack_count, Ordering::Relaxed);

                         // Security: Rate limit NACK processing per peer?
                        if let Some(requesting_peer) = self.get_peer_mut(group_id, &requesting_peer_id).ok() {
                           if let Some(buffer) = requesting_peer.nack_buffer.get(&missing_media_ssrc) {
                                self.metrics.nacks_processed.fetch_add(1, Ordering::Relaxed);
                                let mut packets_found = 0;
                                let mut packets_resent = 0;
                                for seq_no in nack.iter() {
                                    if let Some(p) = buffer.iter().find(|p| p.seq_no == seq_no) {
                                        packets_found += 1;
                                        trace!(recipient=%requesting_peer_id, ssrc=%p.ssrc, seq=%p.seq_no, "Retransmitting NACKed packet");
                                        if requesting_peer.rtc.write_rtp(
                                            p.ssrc, p.pt, p.seq_no, p.marker, p.timestamp, p.time,
                                            p.payload.clone(), ExtensionValues::default(),
                                        ).is_ok() {
                                            packets_resent += 1;
                                        } else {
                                             error!(recipient=%requesting_peer_id, ssrc=%p.ssrc, seq=%p.seq_no, "NACK retransmission failed during write");
                                        }
                                    } else {
                                         trace!(recipient=%requesting_peer_id, ssrc=%missing_media_ssrc, seq=%seq_no, "NACKed packet not found in buffer");
                                    }
                                }
                                self.metrics.nack_packets_found_in_buffer.fetch_add(packets_found, Ordering::Relaxed);
                                self.metrics.nack_packets_retransmitted.fetch_add(packets_resent, Ordering::Relaxed);
                           }
                        }
                    }
                    Ok(Rtcp::PayloadSpecificFeedback(feedback)) if feedback.is_pli() || feedback.is_fir() => {
                        self.metrics.keyframes_requested_in.fetch_add(1, Ordering::Relaxed);
                        let ssrc = feedback.media_ssrc();
                        let kind = if feedback.is_pli() { KeyframeRequestKind::Pli } else { KeyframeRequestKind::Fir };
                        info!(peer = %peer_id, media_ssrc = %ssrc, ?kind, "Keyframe requested by client (PLI/FIR)");
                        self.forward_keyframe_request(group_id, peer_id, ssrc, kind)?;
                    }
                    Ok(_) => { /* Ignore other RTCP */ }
                    Err(e) => warn!(peer = %peer_id, "Failed to parse RTCP packet: {}", e),
                }
            }
            Event::IceConnectionStateChange(state) => {
                info!(peer = %peer_id, ?state, "ICE connection state changed");
                // Update internal state tracker
                if let Some(peer) = self.get_peer_mut(group_id, peer_id).ok() {
                    peer.ice_state = state;
                    if state == IceConnectionState::Connected || state == IceConnectionState::Completed {
                        if peer.connected_time.is_none() { peer.connected_time = Some(Instant::now()); }
                    }
                }
                self.send_event(SfuEvent::IceConnectionStateChange { peer_id: *peer_id, state })?;
            }
            Event::Connected => {
                 info!(peer = %peer_id, "Peer DTLS connection established");
                 if let Some(peer) = self.get_peer_mut(group_id, peer_id).ok() {
                      if peer.connected_time.is_none() { peer.connected_time = Some(Instant::now()); } // Also mark here
                 }
                 self.send_event(SfuEvent::Connected { peer_id: *peer_id })?;
            }
            Event::Candidate(candidate) => {
                 info!(peer = %peer_id, candidate = %candidate.to_sdp(), "Local ICE candidate generated");
                 self.send_event(SfuEvent::IceCandidateReady { peer_id: *peer_id, candidate: candidate.to_sdp() })?;
            }
            Event::KeyframeRequest(req) => {
                info!(peer = %peer_id, ssrc = %req.ssrc, kind = ?req.kind, "Keyframe requested by str0m");
                self.forward_keyframe_request(group_id, peer_id, req.ssrc, req.kind)?;
            }
            _ => { trace!(%peer_id, "Unhandled str0m event: {:?}", event); }
        }
        Ok(())
    }

    fn forward_keyframe_request(&mut self, group_id: &GroupId, requesting_peer_id: &PeerId, ssrc: Ssrc, kind: KeyframeRequestKind) -> Result<(), SfuError> {
         let group = self.groups.get(group_id).ok_or_else(|| SfuError::GroupNotFound(group_id.clone()))?;
         if let Some(origin_peer_id) = group.media_ssrc_origins.get(&ssrc) {
            if origin_peer_id != requesting_peer_id {
                 if let Some(origin_peer) = group.peers.get(origin_peer_id) {
                    info!(origin = %origin_peer_id, target_ssrc = %ssrc, ?kind, "Forwarding Keyframe request");
                    origin_peer.rtc.request_keyframe(ssrc, kind)?;
                    self.metrics.keyframes_requested_out.fetch_add(1, Ordering::Relaxed);
                 } else { return Err(SfuError::PeerNotFound(*origin_peer_id, group_id.clone())); }
            }
         } else { warn!(requester = %requesting_peer_id, target_ssrc = %ssrc, "Keyframe request for unknown SSRC origin"); }
         Ok(())
    }

    fn send_event(&self, event: SfuEvent) -> Result<(), SfuError> {
        if self.event_tx.try_send(event).is_err() {
            error!("Failed to send SFU event, receiver likely dropped");
            Err(SfuError::SendError)
        } else { Ok(()) }
    }
}


// --- Main Application Harness ---
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct CliArgs {
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen_addr: SocketAddr,
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

fn load_config(path: &str) -> Result<SfuConfig, SfuError> {
     match fs::read_to_string(path) {
        Ok(content) => toml::from_str(&content).map_err(|e| SfuError::Config(format!("Failed to parse config file '{}': {}", path, e))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
             warn!("Config file '{}' not found, using default configuration.", path);
             Ok(SfuConfig::default())
        }
        Err(e) => Err(SfuError::Config(format!("Failed to read config file '{}': {}", path, e))),
    }
}


#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli_args = CliArgs::parse();
    let config = Arc::new(load_config(&cli_args.config)?);

    // Initialize logging
    let subscriber = FmtSubscriber::builder().with_max_level(Level::INFO).finish(); // Use INFO by default
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Starting SFU core on {}", cli_args.listen_addr);
    debug!("Using configuration: {:?}", config);

    let udp_socket = Arc::new(UdpSocket::bind(cli_args.listen_addr).await?);
    let local_addr = udp_socket.local_addr()?; // Get local address once
    let mut udp_buf = vec![0u8; config.udp_recv_buf_size]; // Use config for buffer size

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<SfuCommand>(100);
    let (event_tx, mut event_rx) = mpsc::channel::<SfuEvent>(100);
    let metrics = Arc::new(SfuMetrics::default());

    // --- SFU Core Task ---
    let sfu_task_config = config.clone();
    let sfu_task_metrics = metrics.clone();
    let sfu_task = tokio::spawn(async move {
        let mut sfu = Sfu::new(sfu_task_config, sfu_task_metrics, event_tx);
        info!("SFU core task started.");
        while let Some(cmd) = cmd_rx.recv().await {
            sfu.handle_command(cmd);
        }
        info!("SFU core task finished.");
    });

    // --- Event Handling Task ---
    let event_handler_socket = udp_socket.clone();
    let event_handler_metrics = metrics.clone(); // Clone Arc for event handler
    let event_handler_task = tokio::spawn(async move {
        info!("SFU event handler started.");
        while let Some(event) = event_rx.recv().await {
            trace!("Received SFU Event: {:?}", event); // Use trace for frequent events
            match event {
                SfuEvent::TransmitPacket(transmit) => {
                    trace!(dest = %transmit.destination, len = transmit.contents.len(), "Sending UDP packet");
                    if let Err(e) = event_handler_socket.send_to(&transmit.contents, transmit.destination).await {
                        error!("UDP send error: {}", e);
                    }
                }
                SfuEvent::MetricsSnapshot(metrics_snapshot) => {
                    info!("Received Metrics Snapshot Command Response");
                    metrics_snapshot.log_summary(); // Log the snapshot received back
                }
                // Other events previously defined...
                 SfuEvent::SdpAnswerReady { peer_id, answer } => { info!(%peer_id, "Received Answer SDP for peer."); /* Send via signaling */ }
                 SfuEvent::IceCandidateReady { peer_id, candidate } => { info!(%peer_id, %candidate, "Received ICE Candidate for peer."); /* Send via signaling */ }
                 SfuEvent::PeerRemoved { peer_id, group_id, reason } => { info!(%peer_id, %group_id, %reason, "Peer removed event."); }
                 SfuEvent::Error(e) => { error!("SFU Error Event: {}", e); }
                 SfuEvent::Connected { peer_id } => { info!(%peer_id, "Peer Connected event."); }
                 SfuEvent::IceConnectionStateChange { peer_id, state } => { info!(%peer_id, ?state, "Peer ICE State Change event."); }
            }
        }
        info!("SFU event handler finished.");
    });

    // --- Main Loop ---
    let mut interval = tokio::time::interval(config.task_interval); // Use config
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let cmd_tx_main = cmd_tx.clone();
    let metrics_main = metrics.clone(); // Clone Arc for main loop metrics logging

    // Metrics Logging Timer (optional)
    let mut metrics_interval = tokio::time::interval(Duration::from_secs(30));


    loop {
        tokio::select! {
            biased; // Prefer UDP recv slightly

            // --- Handle UDP Input ---
            result = udp_socket.recv_from(&mut udp_buf) => {
                match result {
                    Ok((len, remote_addr)) => {
                        trace!(%remote_addr, %len, "UDP packet received");
                        let data = Bytes::copy_from_slice(&udp_buf[..len]);
                        if cmd_tx_main.send(SfuCommand::HandleUdpPacket{ data, remote_addr, local_addr }).await.is_err() {
                            error!("SFU command channel closed (UDP Handler)"); break;
                        }
                    }
                    Err(e) => { error!("UDP recv error: {}", e); /* Potentially fatal? */ }
                }
            }

            // --- Periodic Tick for SFU ---
            _ = interval.tick() => {
                 if cmd_tx_main.send(SfuCommand::Tick).await.is_err() {
                     error!("SFU command channel closed (Tick Handler)"); break;
                 }
            }

            // --- Periodic Metrics Logging ---
            _ = metrics_interval.tick() => {
                metrics_main.log_summary();
                // Or request snapshot:
                // if cmd_tx_main.send(SfuCommand::GetMetrics).await.is_err() { /* ... */ }
            }

             // --- Graceful Shutdown ---
             _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received, shutting down...");
                drop(cmd_tx_main); drop(cmd_tx);
                break;
            }
        }
    }

    info!("Waiting for tasks to complete...");
    let _ = timeout(Duration::from_secs(2), sfu_task).await;
    let _ = timeout(Duration::from_secs(1), event_handler_task).await;

    info!("Final Metrics:");
    metrics.log_summary(); // Log final metrics
    info!("SFU server stopped.");
    Ok(())
}


// --- Unit Tests ---
#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use str0m::rtp::RtpHeader;
    use std::sync::atomic::Ordering; // For checking metrics

    // --- Test Helpers ---

    #[fixture]
    fn test_setup() -> (Sfu, mpsc::Receiver<SfuEvent>, Arc<SfuConfig>, Arc<SfuMetrics>) {
        let config = Arc::new(SfuConfig::default());
        let metrics = Arc::new(SfuMetrics::default());
        let (event_tx, event_rx) = mpsc::channel(200); // Increased buffer for tests
        let sfu = Sfu::new(config.clone(), metrics.clone(), event_tx);
        (sfu, event_rx, config, metrics)
    }

    async fn expect_event_timeout<F>(event_rx: &mut mpsc::Receiver<SfuEvent>, duration: Duration, condition: F) -> Option<SfuEvent>
    where
        F: Fn(&SfuEvent) -> bool,
    {
        match timeout(duration, async {
            while let Some(event) = event_rx.recv().await {
                if condition(&event) {
                    return Some(event);
                }
            }
            None
        }).await {
            Ok(Some(event)) => Some(event),
            _ => None,
        }
    }
    // Expect specific event type helper
    macro_rules! expect_event_type {
        ($rx:expr, $duration:expr, $pattern:pat $(if $guard:expr)? => $extracted_value:expr) => {
            expect_event_timeout($rx, $duration, |e| matches!(e, $pattern $(if $guard)?)).await.map(|e| {
                if let $pattern $(if $guard)? = e {
                    $extracted_value
                } else {
                    unreachable!("Pattern match failed after successful check")
                }
            })
        };
        ($rx:expr, $pattern:pat $(if $guard:expr)? => $extracted_value:expr) => {
            expect_event_type!($rx, Duration::from_millis(500), $pattern $(if $guard)? => $extracted_value)
        };
         ($rx:expr, $pattern:pat) => {
            expect_event_type!($rx, Duration::from_millis(500), $pattern => ())
        };
         ($rx:expr, $duration: expr, $pattern:pat) => {
            expect_event_type!($rx, $duration, $pattern => ())
        };
    }


    fn minimal_offer_sdp() -> String {
         // Simple offer expecting audio/video, allowing easier candidate parsing
        "v=0\r
o=- 1678886400 1678886400 IN IP4 127.0.0.1\r
s=-\r
t=0 0\r
a=group:BUNDLE 0 1\r
a=msid-semantic: WMS\r
m=audio 9 UDP/TLS/RTP/SAVPF 111\r
c=IN IP4 0.0.0.0\r
a=rtcp:9 IN IP4 0.0.0.0\r
a=ice-ufrag:dummyufrag\r
a=ice-pwd:dummypwd\r
a=ice-options:trickle\r
a=fingerprint:sha-256 00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00\r
a=setup:actpass\r
a=mid:0\r
a=sendrecv\r
a=rtpmap:111 opus/48000/2\r
m=video 9 UDP/TLS/RTP/SAVPF 96\r
c=IN IP4 0.0.0.0\r
a=rtcp:9 IN IP4 0.0.0.0\r
a=ice-ufrag:dummyufrag\r
a=ice-pwd:dummypwd\r
a=ice-options:trickle\r
a=fingerprint:sha-256 00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00\r
a=setup:actpass\r
a=mid:1\r
a=sendrecv\r
a=rtpmap:96 VP8/90000\r
".to_string()
    }

    // Helper to manually establish connection state for tests
    // This is a simplification; real ICE is complex.
    fn simulate_connection(sfu: &mut Sfu, group_id: &str, peer_id: PeerId, remote_addr: SocketAddr) {
        // Manually set peer address and trigger connected state event if peer exists
        if let Ok(peer) = sfu.get_peer_mut(&group_id.to_string(), &peer_id) {
            peer.remote_addr = Some(remote_addr);
            peer.ice_state = IceConnectionState::Connected; // Force state
            peer.connected_time = Some(Instant::now());
            sfu.peers_by_addr.insert(remote_addr, peer_id); // Add mapping

             // Inject a connected event to ensure state is consistent internally if needed
             let connected_event = Event::Connected;
             let _ = sfu.handle_str0m_event(&group_id.to_string(), &peer_id, connected_event);
        } else {
            panic!("Peer {} not found during simulate_connection", peer_id);
        }
    }

     // Helper to create MediaData for testing
    fn create_media_data(ssrc: u32, pt: u8, seq_no: u16, timestamp: u32, payload: &[u8]) -> MediaData {
        use str0m::rtp::{RtpHeader, RtpPacket}; // Need RtpPacket to build MediaData easily

        let mut header = RtpHeader::new(pt, seq_no, timestamp, Ssrc::from(ssrc));
        // Can set marker bit if needed: header.marker = true;
        let packet = RtpPacket {
            header,
            payload: Bytes::copy_from_slice(payload),
            timestamp: MediaTime::from_secs(0), // Placeholder time
            transport_seq_no: None,
        };
        // Convert RtpPacket to MediaData (requires access to internal fields or helper)
        // As str0m doesn't expose a direct RtpPacket->MediaData constructor,
        // we might need to simulate the input path differently or rely on forwarding logic
        // to create the MediaData structure implicitly when testing forwarding.
        // Let's assume for testing `handle_str0m_event` we can construct it:
         MediaData {
             mid: Mid::from_index(0), // Assume MID 0 for simplicity
             pt,
             seq_no,
             timestamp,
             time: MediaTime::from_secs(0),
             marker: false,
             ssrc: Ssrc::from(ssrc),
             payload: Bytes::copy_from_slice(payload),
             // extensions: ExtensionValues::default(), // Need to construct if testing extensions
             // Padding/Payload Type Context might be needed for str0m internals if directly injecting MediaData event
         }

    }

    // Helper to create RTCP NACK packet bytes
     fn create_nack_bytes(sender_ssrc: u32, media_ssrc: u32, missing_seqs: &[u16]) -> Bytes {
        use str0m::rtcp::{Nack, RtcpPacket, TransportLayerFeedback};
        use bytes::BytesMut;

        let mut nack = Nack::new(Ssrc::from(media_ssrc));
        for seq in missing_seqs {
            nack.add(*seq);
        }
        let feedback = TransportLayerFeedback::Nack(nack);
        let packet = RtcpPacket::TransportLayerFeedback(feedback);

        let mut buf = BytesMut::with_capacity(64); // Adjust capacity
        packet.write_to(&mut buf).expect("Failed to write NACK to buffer");
        buf.freeze()
    }


    // --- Tests ---

    #[rstest]
    #[tokio::test]
    async fn test_add_peer_and_config(mut test_sfu: (Sfu, mpsc::Receiver<SfuEvent>, Arc<SfuConfig>, Arc<SfuMetrics>)) {
        let (mut sfu, mut event_rx, config, metrics) = test_sfu;
        let group_id = "test_group_config".to_string();
        let peer_id = Uuid::new_v4();

        assert_eq!(sfu.config.rtp_packet_buffer_size, config.rtp_packet_buffer_size); // Check config applied

        let result = sfu.add_peer(group_id.clone(), peer_id, minimal_offer_sdp());
        assert!(result.is_ok());
        assert_eq!(metrics.peers_added.load(Ordering::Relaxed), 1);

        let answer_event = expect_event_type!( &mut event_rx, SfuEvent::SdpAnswerReady { peer_id: pid, .. } => pid ).await;
        assert_eq!(answer_event, Some(peer_id));
    }

    #[rstest]
    #[tokio::test]
    async fn test_remove_peer_metrics(mut test_sfu: (Sfu, mpsc::Receiver<SfuEvent>, Arc<SfuConfig>, Arc<SfuMetrics>)) {
        let (mut sfu, mut event_rx, _config, metrics) = test_sfu;
        let group_id = "test_group_remove_metrics".to_string();
        let peer_id = Uuid::new_v4();

        sfu.add_peer(group_id.clone(), peer_id, minimal_offer_sdp()).unwrap();
        expect_event_type!(&mut event_rx, SfuEvent::SdpAnswerReady).await; // Consume answer

        assert_eq!(metrics.peers_added.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.peers_removed.load(Ordering::Relaxed), 0);

        sfu.remove_peer(&peer_id, "test_removal".to_string());

        assert!(!sfu.groups.contains_key(&group_id));
        assert_eq!(metrics.peers_added.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.peers_removed.load(Ordering::Relaxed), 1);

        let removed_event = expect_event_type!(&mut event_rx, SfuEvent::PeerRemoved { peer_id: pid, reason: r, .. } => (pid, r)).await;
        assert_eq!(removed_event, Some((peer_id, "test_removal".to_string())));
    }


    #[rstest]
    #[tokio::test]
    async fn test_rtp_forwarding_and_metrics(mut test_sfu: (Sfu, mpsc::Receiver<SfuEvent>, Arc<SfuConfig>, Arc<SfuMetrics>)) {
        let (mut sfu, mut event_rx, _config, metrics) = test_sfu;
        let group_id = "fwd_group_metrics".to_string();
        let peer_a_id = Uuid::new_v4();
        let peer_b_id = Uuid::new_v4();
        let addr_a: SocketAddr = "1.1.1.1:1111".parse().unwrap();
        let addr_b: SocketAddr = "2.2.2.2:2222".parse().unwrap();
        let local_addr: SocketAddr = "192.168.1.100:3000".parse().unwrap(); // Simulated local addr

        // 1. Add peers
        sfu.add_peer(group_id.clone(), peer_a_id, minimal_offer_sdp()).unwrap();
        expect_event_type!(&mut event_rx, SfuEvent::SdpAnswerReady { peer_id: p } if p == peer_a_id).await;
        sfu.add_peer(group_id.clone(), peer_b_id, minimal_offer_sdp()).unwrap();
        expect_event_type!(&mut event_rx, SfuEvent::SdpAnswerReady { peer_id: p } if p == peer_b_id).await;

        // 2. Simulate connection
        simulate_connection(&mut sfu, &group_id, peer_a_id, addr_a);
        simulate_connection(&mut sfu, &group_id, peer_b_id, addr_b);

        // 3. Create RTP packet data
        let rtp_ssrc = 1111_u32;
        let rtp_payload = b"forward_me";
        let rtp_seq = 100_u16;

        // 4. Simulate receiving packet from Peer A via HandleUdpPacket
        // This requires constructing bytes that str0m can parse as the intended RTP packet.
        // Direct event injection `handle_str0m_event` is easier for testing core logic.
        let media_data = create_media_data(rtp_ssrc, 96, rtp_seq, 1000, rtp_payload);
         // Directly inject the event for easier testing of forwarding logic
        sfu.handle_str0m_event(&group_id, &peer_a_id, Event::MediaData(media_data)).unwrap();

        // 5. Poll SFU (needed if using HandleUdpPacket, less critical for direct event inject)
        sfu.poll_and_forward(); // Process potential buffer writes/etc.

        // 6. Assert Transmit event for Peer B
        let transmit_event = expect_event_type!(&mut event_rx, Duration::from_millis(100), SfuEvent::TransmitPacket(t) => t).await;

        assert!(transmit_event.is_some(), "Did not receive expected TransmitPacket event");
        if let Some(transmit) = transmit_event {
            assert_eq!(transmit.destination, addr_b);
            assert!(transmit.contents.len() > rtp_payload.len()); // Should include header

            // Try parsing the transmitted packet header (basic check)
            let rtp_res = RtpHeader::parse(&transmit.contents);
            assert!(rtp_res.is_ok(), "Transmitted packet is not valid RTP");
            if let Ok(header) = rtp_res {
                assert_eq!(header.ssrc, Ssrc::from(rtp_ssrc)); // SSRC must be preserved
                // Note: Sequence number *might* be rewritten by str0m's Rtc when forwarding.
                // Asserting payload type or marker might be more reliable if needed.
                 assert_eq!(header.payload_type, 96);
            }

            // Check Metrics
             assert_eq!(metrics.rtp_received.load(Ordering::Relaxed), 1);
             assert_eq!(metrics.rtp_bytes_in.load(Ordering::Relaxed), rtp_payload.len());
             assert_eq!(metrics.rtp_forwarded_attempts.load(Ordering::Relaxed), 1); // One attempt to B
             assert_eq!(metrics.rtp_forwarded_success.load(Ordering::Relaxed), 1); // Assumes write_rtp succeeded
             assert_eq!(metrics.rtp_bytes_forwarded.load(Ordering::Relaxed), rtp_payload.len());
             assert_eq!(metrics.udp_packets_out.load(Ordering::Relaxed), 1); // Check UDP out counter
             assert_eq!(metrics.udp_bytes_out.load(Ordering::Relaxed), transmit.contents.len());
        }
    }


    #[rstest]
    #[tokio::test]
    async fn test_nack_retransmission(mut test_sfu: (Sfu, mpsc::Receiver<SfuEvent>, Arc<SfuConfig>, Arc<SfuMetrics>)) {
        let (mut sfu, mut event_rx, _config, metrics) = test_sfu;
        let group_id = "nack_group".to_string();
        let peer_a_id = Uuid::new_v4(); // Sender
        let peer_b_id = Uuid::new_v4(); // Receiver/NACKer
        let addr_a: SocketAddr = "3.3.3.3:3333".parse().unwrap();
        let addr_b: SocketAddr = "4.4.4.4:4444".parse().unwrap();
        let local_addr: SocketAddr = "192.168.1.100:3000".parse().unwrap();

        // 1. Setup peers & connection
        sfu.add_peer(group_id.clone(), peer_a_id, minimal_offer_sdp()).unwrap();
        expect_event_type!(&mut event_rx, SfuEvent::SdpAnswerReady { peer_id: p } if p == peer_a_id).await;
        sfu.add_peer(group_id.clone(), peer_b_id, minimal_offer_sdp()).unwrap();
        expect_event_type!(&mut event_rx, SfuEvent::SdpAnswerReady { peer_id: p } if p == peer_b_id).await;
        simulate_connection(&mut sfu, &group_id, peer_a_id, addr_a);
        simulate_connection(&mut sfu, &group_id, peer_b_id, addr_b);

        // 2. Send packet 100 from A, forward to B (and buffer for B)
        let rtp_ssrc = 2222_u32;
        let payload100 = b"packet_100";
        let media100 = create_media_data(rtp_ssrc, 96, 100, 1000, payload100);
        sfu.handle_str0m_event(&group_id, &peer_a_id, Event::MediaData(media100)).unwrap();
        expect_event_type!(&mut event_rx, Duration::from_millis(100), SfuEvent::TransmitPacket(t) if t.destination == addr_b).await; // Consume forward

         // Verify packet 100 is buffered for Peer B under SSRC 2222
        let peer_b_state = sfu.groups[&group_id].peers.get(&peer_b_id).unwrap();
        assert!(peer_b_state.nack_buffer.contains_key(&Ssrc::from(rtp_ssrc)));
        assert_eq!(peer_b_state.nack_buffer[&Ssrc::from(rtp_ssrc)].len(), 1);
        assert_eq!(peer_b_state.nack_buffer[&Ssrc::from(rtp_ssrc)][0].seq_no, 100);

        // 3. Send packet 102 from A, forward to B (and buffer for B)
        let payload102 = b"packet_102";
        let media102 = create_media_data(rtp_ssrc, 96, 102, 1020, payload102);
        sfu.handle_str0m_event(&group_id, &peer_a_id, Event::MediaData(media102)).unwrap();
        expect_event_type!(&mut event_rx, Duration::from_millis(100), SfuEvent::TransmitPacket(t) if t.destination == addr_b).await; // Consume forward

        // 4. Simulate receiving NACK from Peer B requesting packet 101
        let nack_seqs = [101]; // The missing packet
        let nack_bytes = create_nack_bytes(0, rtp_ssrc, &nack_seqs); // Sender SSRC in RTCP header often 0 or peer's RTCP SSRC
        let rtcp_event = Event::RtcpData(str0m::rtcp::RtcpData {
             // Need to construct RtcpData correctly, might need more fields based on str0m version
             mid: Mid::from_index(0), // Assume MID 0
             pt: 0, // NACK doesn't have RTP PT
             ssrc: Ssrc::from(3333), // SSRC of the peer *sending* the RTCP packet (Peer B's RTCP SSRC)
             payload: nack_bytes,
        });
        sfu.handle_str0m_event(&group_id, &peer_b_id, rtcp_event).unwrap();

        // 5. Poll SFU
        sfu.poll_and_forward();

        // 6. Assert Transmit event for Peer B containing the *retransmitted* packet 101 (which wasn't found)
        // In this specific test case, packet 101 was *never sent*, so it shouldn't be found in buffer.
        // Let's modify the test: send 100, 101, then NACK for 100.

        // --- Revised NACK Test ---
        let (mut sfu, mut event_rx, _config, metrics) = test_setup().await; // Resetup
        let group_id = "nack_group_revised".to_string();
        let peer_a_id = Uuid::new_v4(); let peer_b_id = Uuid::new_v4();
        let addr_a: SocketAddr = "3.3.3.3:3333".parse().unwrap(); let addr_b: SocketAddr = "4.4.4.4:4444".parse().unwrap();
        sfu.add_peer(group_id.clone(), peer_a_id, minimal_offer_sdp()).unwrap();
        expect_event_type!(&mut event_rx, SfuEvent::SdpAnswerReady { peer_id: p } if p == peer_a_id).await;
        sfu.add_peer(group_id.clone(), peer_b_id, minimal_offer_sdp()).unwrap();
        expect_event_type!(&mut event_rx, SfuEvent::SdpAnswerReady { peer_id: p } if p == peer_b_id).await;
        simulate_connection(&mut sfu, &group_id, peer_a_id, addr_a);
        simulate_connection(&mut sfu, &group_id, peer_b_id, addr_b);

        let rtp_ssrc = 4444_u32;
        // Send packet 100
        let payload100 = b"nack_100";
        let media100 = create_media_data(rtp_ssrc, 96, 100, 2000, payload100);
        sfu.handle_str0m_event(&group_id, &peer_a_id, Event::MediaData(media100)).unwrap();
        expect_event_type!(&mut event_rx, Duration::from_millis(100), SfuEvent::TransmitPacket(t) if t.destination == addr_b).await;
        // Send packet 101
        let payload101 = b"nack_101";
        let media101 = create_media_data(rtp_ssrc, 96, 101, 2010, payload101);
        sfu.handle_str0m_event(&group_id, &peer_a_id, Event::MediaData(media101)).unwrap();
        expect_event_type!(&mut event_rx, Duration::from_millis(100), SfuEvent::TransmitPacket(t) if t.destination == addr_b).await;

         // Verify buffer for Peer B
        let peer_b_state = sfu.groups[&group_id].peers.get(&peer_b_id).unwrap();
        assert_eq!(peer_b_state.nack_buffer[&Ssrc::from(rtp_ssrc)].len(), 2);
        assert_eq!(peer_b_state.nack_buffer[&Ssrc::from(rtp_ssrc)][0].seq_no, 100);
        assert_eq!(peer_b_state.nack_buffer[&Ssrc::from(rtp_ssrc)][1].seq_no, 101);


        // Simulate NACK from Peer B for packet 100
        let nack_seqs = [100];
        let nack_bytes = create_nack_bytes(0, rtp_ssrc, &nack_seqs);
        let rtcp_event = Event::RtcpData(str0m::rtcp::RtcpData { mid: Mid::from_index(0), pt: 0, ssrc: Ssrc::from(5555), payload: nack_bytes }); // Peer B's RTCP SSRC 5555
        sfu.handle_str0m_event(&group_id, &peer_b_id, rtcp_event).unwrap();

         // Poll SFU
        sfu.poll_and_forward();

        // Assert Transmit event for Peer B containing retransmitted packet 100
        let retransmit_event = expect_event_type!(&mut event_rx, Duration::from_millis(100), SfuEvent::TransmitPacket(t) => t).await;
        assert!(retransmit_event.is_some(), "Did not receive NACK retransmission");
        if let Some(transmit) = retransmit_event {
            assert_eq!(transmit.destination, addr_b);
            let rtp_res = RtpHeader::parse(&transmit.contents);
            assert!(rtp_res.is_ok(), "Retransmitted packet is not valid RTP");
            if let Ok(header) = rtp_res {
                assert_eq!(header.ssrc, Ssrc::from(rtp_ssrc));
                assert_eq!(header.sequence_number, 100); // Check sequence number specifically for NACK
                assert_eq!(header.payload_type, 96);
                // Check if payload matches original payload100 (requires parsing beyond header)
                 let payload_offset = header.header_len();
                 assert_eq!(&transmit.contents[payload_offset..payload_offset + payload100.len()], payload100);
            }
             // Check Metrics
             assert_eq!(metrics.nacks_received.load(Ordering::Relaxed), 1);
             assert_eq!(metrics.nacks_processed.load(Ordering::Relaxed), 1);
             assert_eq!(metrics.nack_packets_requested.load(Ordering::Relaxed), 1);
             assert_eq!(metrics.nack_packets_found_in_buffer.load(Ordering::Relaxed), 1);
             assert_eq!(metrics.nack_packets_retransmitted.load(Ordering::Relaxed), 1);
             assert_eq!(metrics.udp_packets_out.load(Ordering::Relaxed), 3); // 2 original forwards + 1 retransmit
        }
    }
}
