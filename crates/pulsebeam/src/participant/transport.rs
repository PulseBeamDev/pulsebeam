use std::collections::VecDeque;
use std::net::SocketAddr;

use crate::participant::batcher::{Batcher, NetworkEgress, OwnedPacketQueue};
use crate::rtp::{Codec as RtpCodec, RtpPacket};
use pulsebeam_runtime::net::RecvPacketBatch;
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaKind, MediaTime, Mid, Pt, Rid};
use str0m::net::Protocol;
use str0m::rtp::RtpWrite;
use str0m::{Event, Output, Rtc, RtcError};
use tokio::time::Instant;

fn egress_extension_values(
    mut ext_vals: str0m::rtp::ExtensionValues,
    playout_delay: Option<(MediaTime, MediaTime)>,
) -> str0m::rtp::ExtensionValues {
    ext_vals.mid = None;
    ext_vals.rid = None;
    ext_vals
        .user_values
        .remove::<str0m::rtp::vla::VideoLayersAllocation>();
    if let Some((min, max)) = playout_delay {
        ext_vals.play_delay_min = Some(min);
        ext_vals.play_delay_max = Some(max);
    }
    ext_vals
}

pub(crate) const MAX_PENDING_INGRESS: usize = 256;
pub(crate) const MAX_PENDING_FANOUT: usize = 256;

pub(crate) enum TransportPollOutput {
    Timeout(Instant),
    Transmit,
    Event(Box<Event>),
}

pub(crate) enum IngressResult {
    Empty,
    Malformed(SocketAddr),
    Received,
}

pub(crate) enum TransportMutation {
    Data {
        channel: ChannelId,
        bytes: Vec<u8>,
    },
    Keyframe {
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    },
}

pub(crate) struct RtpWriteCommand {
    pub(crate) pkt: RtpPacket,
    pub(crate) mid: Mid,
    pub(crate) rid: Option<Rid>,
    pub(crate) ssrc: str0m::rtp::Ssrc,
    pub(crate) pt: Pt,
    pub(crate) kind: MediaKind,
    pub(crate) now: Instant,
    pub(crate) playout_delay: Option<(MediaTime, MediaTime)>,
}

pub(crate) struct RtpIngress {
    pub(crate) mid: Mid,
    pub(crate) rid: Option<Rid>,
    pub(crate) packet: RtpPacket,
    pub(crate) sender_info: Option<str0m::rtp::rtcp::SenderInfo>,
}

pub(crate) enum AppliedMutation {
    Applied,
    KeyframeUnavailable {
        mid: Mid,
        rid: Option<Rid>,
    },
    KeyframeRequested {
        mid: Mid,
        rid: Option<Rid>,
    },
    RtpNotWritten,
    RtpWritten,
    RecoveredStream {
        kind: MediaKind,
        mid: Mid,
        rid: Option<Rid>,
        ssrc: str0m::rtp::Ssrc,
    },
}

pub(crate) struct Transport {
    rtc: Rtc,
    udp_packets: OwnedPacketQueue,
    tcp_batcher: Batcher,
    pending_ingress: VecDeque<RecvPacketBatch>,
    pending_timeout: Option<Instant>,
    pending_mutations: VecDeque<TransportMutation>,
    last_ingress: Option<(SocketAddr, SocketAddr)>,
    last_ingress_shard: Option<crate::id::ShardId>,
    rtc_deadline: Option<Instant>,
    rtc_clock: Instant,
    rtc_needs_drain: bool,
    exited: bool,
    #[cfg(debug_assertions)]
    egress_guard: crate::rtp::egress_guard::EgressGuard,
    #[cfg(feature = "sim")]
    pub(crate) sim_span: tracing::Span,
}

impl Transport {
    pub(crate) fn new(
        rtc: Rtc,
        udp_gso_size: usize,
        tcp_gso_size: usize,
        now: Instant,
        #[cfg(feature = "sim")] sim_span: tracing::Span,
    ) -> Self {
        Self {
            rtc,
            udp_packets: OwnedPacketQueue::with_capacity(udp_gso_size),
            tcp_batcher: Batcher::with_capacity(tcp_gso_size),
            pending_ingress: VecDeque::new(),
            pending_timeout: None,
            pending_mutations: VecDeque::new(),
            last_ingress: None,
            last_ingress_shard: None,
            rtc_deadline: None,
            rtc_clock: now,
            rtc_needs_drain: true,
            exited: false,
            #[cfg(debug_assertions)]
            egress_guard: crate::rtp::egress_guard::EgressGuard::new(),
            #[cfg(feature = "sim")]
            sim_span,
        }
    }

    pub(crate) fn enqueue_ingress(
        &mut self,
        batch: RecvPacketBatch,
        source_shard: crate::id::ShardId,
    ) {
        self.last_ingress = Some((batch.src, batch.dst));
        self.last_ingress_shard = Some(source_shard);
        if self.pending_ingress.len() >= MAX_PENDING_INGRESS {
            let _ = self.pending_ingress.pop_front();
            metrics::counter!("participant_ingress_shed").increment(1);
        }
        self.pending_ingress.push_back(batch);
    }

    pub(crate) fn enqueue_timeout(&mut self, now: Instant) {
        self.pending_timeout = Some(now);
    }

    pub(crate) fn enqueue_mutation(&mut self, mutation: TransportMutation) -> bool {
        if self.pending_mutations.len() >= MAX_PENDING_FANOUT {
            metrics::counter!("participant_fanout_shed").increment(1);
            return false;
        }
        self.pending_mutations.push_back(mutation);
        true
    }

    pub(crate) fn apply_next_mutation(&mut self, _now: Instant) -> Option<AppliedMutation> {
        let mutation = self.pending_mutations.pop_front()?;
        let result = match mutation {
            TransportMutation::Data { channel, bytes } => {
                let _ = self.write_channel(channel, true, &bytes);
                AppliedMutation::Applied
            }
            TransportMutation::Keyframe { mid, rid, kind } => {
                if self.request_keyframe(KeyframeRequest { mid, rid, kind }) {
                    AppliedMutation::KeyframeRequested { mid, rid }
                } else {
                    AppliedMutation::KeyframeUnavailable { mid, rid }
                }
            }
        };
        Some(result)
    }

    pub(crate) fn drain_network(&mut self, egress: &mut impl NetworkEgress) {
        loop {
            match egress.append_udp(&mut self.udp_packets) {
                crate::participant::batcher::AppendStatus::Drained => break,
                crate::participant::batcher::AppendStatus::Full if !egress.flush() => break,
                crate::participant::batcher::AppendStatus::Full => {}
            }
        }
        loop {
            match egress.append_tcp(&mut self.tcp_batcher) {
                crate::participant::batcher::AppendStatus::Drained => break,
                crate::participant::batcher::AppendStatus::Full if !egress.flush() => break,
                crate::participant::batcher::AppendStatus::Full => {}
            }
        }
    }

    pub(crate) fn poll_output(&mut self) -> Result<Option<TransportPollOutput>, RtcError> {
        if !self.rtc.is_alive() {
            return Ok(None);
        }
        match self.rtc.poll_output()? {
            Output::Timeout(deadline) => Ok(Some(TransportPollOutput::Timeout(deadline.into()))),
            Output::Transmit(tx) => {
                match tx.proto {
                    Protocol::Udp => self
                        .udp_packets
                        .push_back(tx.destination, tx.contents.into()),
                    Protocol::Tcp => self.tcp_batcher.push_back(tx.destination, &tx.contents),
                    _ => {}
                }
                Ok(Some(TransportPollOutput::Transmit))
            }
            Output::Event(event) => Ok(Some(TransportPollOutput::Event(Box::new(event)))),
        }
    }

    pub(crate) fn write_channel(&mut self, cid: ChannelId, binary: bool, bytes: &[u8]) -> bool {
        let Some(mut channel) = self.rtc.channel(cid) else {
            return false;
        };
        channel.write(binary, bytes).is_ok()
    }

    pub(crate) fn channel_config(
        &mut self,
        cid: ChannelId,
    ) -> Option<str0m::channel::ChannelConfig> {
        self.rtc.channel(cid)?.config().cloned()
    }

    pub(crate) fn preferred_send_pt(&self, mid: Mid, kind: MediaKind) -> Option<Pt> {
        let media = self.rtc.media(mid)?;
        let remote_pts = media.remote_pts();
        if remote_pts.is_empty() {
            return None;
        }
        let expected_codec = match kind {
            MediaKind::Audio => Codec::Opus,
            MediaKind::Video => Codec::H264,
        };
        let codec_config = self.rtc.codec_config();
        remote_pts
            .iter()
            .copied()
            .find(|pt| {
                codec_config
                    .params()
                    .iter()
                    .any(|params| params.pt() == *pt && params.spec().codec == expected_codec)
            })
            .or_else(|| {
                kind.is_video()
                    .then(|| remote_pts.first().copied())
                    .flatten()
            })
    }

    pub(crate) fn stream_tx_ssrc(&mut self, mid: Mid) -> Option<str0m::rtp::Ssrc> {
        self.rtc
            .direct_api()
            .stream_tx_by_mid(mid, None)
            .map(|stream| stream.ssrc())
    }

    pub(crate) fn lookup_rtp(&mut self, rtp: str0m::rtp::RtpPacket) -> Option<RtpIngress> {
        let ssrc = rtp.header.ssrc;
        let (mid, rid) = {
            let mut api = self.rtc.direct_api();
            let stream = api.stream_rx(&ssrc)?;
            (stream.mid(), stream.rid())
        };
        self.convert_rtp(rtp, mid, rid)
    }

    pub(crate) fn convert_rtp(
        &mut self,
        rtp: str0m::rtp::RtpPacket,
        mid: Mid,
        rid: Option<Rid>,
    ) -> Option<RtpIngress> {
        let media = self.rtc.media(mid)?;
        let kind = media.kind();
        let codec = match kind {
            MediaKind::Audio => RtpCodec::Opus,
            MediaKind::Video => RtpCodec::H264,
        };
        let (packet, sender_info) = RtpPacket::from_str0m(rtp, codec);
        Some(RtpIngress {
            mid,
            rid,
            packet,
            sender_info,
        })
    }

    pub(crate) fn with_bwe<R>(&mut self, f: impl FnOnce(&mut str0m::bwe::Bwe) -> R) -> R {
        f(&mut self.rtc.bwe())
    }

    pub(crate) fn connection_context(
        &self,
    ) -> (Option<(SocketAddr, SocketAddr)>, Option<crate::id::ShardId>) {
        (self.last_ingress, self.last_ingress_shard)
    }

    pub(crate) fn has_pending_ingress(&self) -> bool {
        !self.pending_ingress.is_empty()
    }

    pub(crate) fn clock(&self) -> Instant {
        self.rtc_clock
    }

    pub(crate) fn advance_clock(&mut self, candidate: Instant, wall_now: Instant) {
        let previous = self.rtc_clock;
        self.rtc_clock = self.rtc_clock.max(candidate).max(wall_now);
        debug_assert!(
            self.rtc_clock >= previous,
            "transport RTC clock moved backwards"
        );
        debug_assert!(
            self.rtc_clock
                <= wall_now
                    .checked_add(pulsebeam_runtime::SHARD_TIMER_QUANTUM)
                    .unwrap_or(wall_now),
            "transport RTC clock advanced beyond one wheel quantum"
        );
    }

    pub(crate) fn needs_drain(&self) -> bool {
        self.rtc_needs_drain
    }

    pub(crate) fn mark_needs_drain(&mut self) {
        self.rtc_needs_drain = true;
    }

    pub(crate) fn set_drain_result(&mut self, deadline: Option<Instant>) -> bool {
        let Some(deadline) = deadline else {
            self.rtc_deadline = None;
            self.rtc_needs_drain = false;
            self.exited = true;
            return false;
        };
        self.rtc_deadline = Some(deadline);
        self.rtc_needs_drain = false;
        true
    }

    pub(crate) fn take_timeout(&mut self) -> Option<Instant> {
        self.pending_timeout.take()
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.rtc_deadline
    }

    pub(crate) fn is_exited(&self) -> bool {
        self.exited
    }

    pub(crate) fn request_keyframe(&mut self, request: KeyframeRequest) -> bool {
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_rx_by_mid(request.mid, request.rid) else {
            return false;
        };
        stream.request_keyframe(request.kind);
        true
    }

    fn write_rtp(&mut self, command: RtpWriteCommand) -> AppliedMutation {
        let RtpWriteCommand {
            pkt,
            mid,
            rid,
            ssrc: requested_ssrc,
            pt,
            kind,
            now,
            playout_delay,
        } = command;
        let nackable = kind == MediaKind::Video;
        let mut api = self.rtc.direct_api();
        let (stream, recovered) = match api.stream_tx(&requested_ssrc) {
            Some(stream) if stream.mid() == mid && stream.rid() == rid => (Some(stream), false),
            Some(stream) => {
                debug_assert!(stream.mid() != mid || stream.rid() != rid);
                (api.stream_tx_by_mid(mid, rid), true)
            }
            None => (api.stream_tx_by_mid(mid, rid), true),
        };
        let Some(stream) = stream else {
            if nackable {
                tracing::warn!(target: crate::log::TARGET_VIDEO, %mid, ?rid, "no stream_tx_by_mid found");
            } else {
                tracing::warn!(target: crate::log::TARGET_AUDIO, %mid, "no stream_tx_by_mid found");
            }
            return AppliedMutation::RtpNotWritten;
        };
        debug_assert_eq!(stream.mid(), mid);
        debug_assert_eq!(stream.rid(), rid);
        let ssrc = stream.ssrc();
        #[cfg(debug_assertions)]
        if let Some(violation) =
            self.egress_guard
                .check(mid, rid, *pkt.seq_no, pkt.rtp_ts.numer(), pkt.marker, kind)
        {
            tracing::error!(%mid, ?rid, %violation, "egress stream invariant violated");
            #[cfg(feature = "sim")]
            pulsebeam_runtime::fatal!("egress stream invariant violated: {violation}");
        }
        let ext_vals = egress_extension_values(pkt.ext_vals, playout_delay);
        if nackable {
            tracing::trace!(
                target: crate::log::TARGET_VIDEO,
                %mid,
                ?rid,
                %ssrc,
                %pt,
                seq = %pkt.seq_no,
                len = pkt.payload.len(),
                marker = pkt.marker,
                "Writing RTP packet"
            );
        } else {
            tracing::trace!(
                target: crate::log::TARGET_AUDIO,
                %mid,
                %ssrc,
                %pt,
                seq = %pkt.seq_no,
                len = pkt.payload.len(),
                marker = pkt.marker,
                "Writing RTP packet"
            );
        }
        let rtp = RtpWrite::new(
            pt,
            pkt.seq_no,
            u32::try_from(pkt.rtp_ts.numer() & u64::from(u32::MAX)).unwrap_or(0),
            now.into(),
            pkt.payload,
        )
        .nackable(nackable)
        .marker(pkt.marker)
        .ext_vals(ext_vals);
        stream.write_rtp(rtp);
        if recovered {
            AppliedMutation::RecoveredStream {
                kind,
                mid,
                rid,
                ssrc,
            }
        } else {
            AppliedMutation::RtpWritten
        }
    }

    pub(crate) fn apply_rtp_command(&mut self, command: RtpWriteCommand) -> AppliedMutation {
        self.write_rtp(command)
    }

    pub(crate) fn receive_pending(&mut self, at: Instant) -> IngressResult {
        let Some(batch) = self.pending_ingress.front_mut() else {
            return IngressResult::Empty;
        };
        let proto = match batch.transport {
            pulsebeam_runtime::net::Transport::Udp(_) => Protocol::Udp,
            pulsebeam_runtime::net::Transport::Tcp => Protocol::Tcp,
        };
        let source = batch.src;
        let destination = batch.dst;
        let Some(packet) = batch.next_packet() else {
            self.pending_ingress.pop_front();
            return IngressResult::Empty;
        };
        let Ok(contents) = packet.try_into() else {
            self.pending_ingress.pop_front();
            return IngressResult::Malformed(source);
        };
        let receive = str0m::net::Receive {
            proto,
            source,
            destination,
            contents,
        };
        let _ = self
            .rtc
            .handle_input(str0m::Input::Receive(at.into(), receive));
        self.rtc_needs_drain = true;
        IngressResult::Received
    }

    pub(crate) fn timeout(&mut self) {
        let _ = self
            .rtc
            .handle_input(str0m::Input::Timeout(self.rtc_clock.into()));
        self.rtc_needs_drain = true;
    }

    pub(crate) fn disconnect(&mut self) {
        self.rtc.disconnect();
        self.rtc_needs_drain = true;
    }
}

#[cfg(test)]
mod tests {
    use super::egress_extension_values;
    use str0m::media::{Mid, Rid};
    use str0m::rtp::ExtensionValues;

    #[test]
    fn ingress_stream_identity_is_not_reused_on_egress() {
        let mut ingress = ExtensionValues::default();
        ingress.mid = Some(Mid::from("source"));
        ingress.rid = Some(Rid::from("h"));

        let egress = egress_extension_values(ingress, None);

        assert_eq!(egress.mid, None);
        assert_eq!(egress.rid, None);
    }
}
