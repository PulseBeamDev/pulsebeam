use str0m::{
    Input, Output, Rtc, RtcError,
    channel::ChannelId,
    media::{KeyframeRequest, MediaData},
    net::Receive,
};
use tokio::time::Instant;

use pulsebeam_runtime::net;

/// Manages all direct interactions with the str0m WebRTC engine.
pub struct WebRtcHandler {
    rtc: Rtc,
}

impl WebRtcHandler {
    pub fn new(rtc: Rtc) -> Self {
        Self { rtc }
    }

    #[inline]
    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    pub fn disconnect(&mut self) {
        tracing::info!("WebRTC handler disconnecting.");
        self.rtc.disconnect();
    }

    pub fn channel(&mut self, cid: ChannelId) -> Option<str0m::channel::Channel<'_>> {
        self.rtc.channel(cid)
    }

    /// HOT PATH (INGRESS): Handles an incoming network packet.
    #[inline]
    pub fn handle_input_packet(&mut self, packet: net::RecvPacket) {
        let now = Instant::now();
        let res = self.rtc.handle_input(Input::Receive(
            now.into_std(),
            Receive {
                proto: str0m::net::Protocol::Udp,
                source: packet.src,
                destination: packet.dst,
                contents: (&*packet.buf).try_into().unwrap(),
            },
        ));

        if let Err(err) = res {
            tracing::debug!("str0m dropped a UDP packet: {err}");
        }
    }

    /// Polls the WebRTC engine for the next output event or timeout.
    #[inline]
    pub fn poll_output(&mut self) -> Result<Output, RtcError> {
        self.rtc.poll_output()
    }

    /// Informs the engine that a timeout has occurred.
    #[inline]
    pub fn handle_timeout(&mut self, now: std::time::Instant) {
        if let Err(e) = self.rtc.handle_input(Input::Timeout(now)) {
            tracing::error!("Failed to handle RTC timeout: {}", e);
        }
    }

    /// HOT PATH (EGRESS): Writes media data to a specific outgoing Mid.
    #[inline]
    pub fn write_media(&mut self, mid: str0m::media::Mid, data: &MediaData) {
        let writer = match self.rtc.writer(mid) {
            Some(writer) => writer,
            None => return, // Client is not subscribed to this Mid.
        };

        let pt = match writer.match_params(data.params) {
            Some(pt) => pt,
            None => {
                tracing::trace!(?mid, ?data.params, "Payload type mismatch, dropping packet");
                return;
            }
        };

        // Cloning the Bytes here is cheap (it's an Arc internally).
        if let Err(err) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
            tracing::error!("Failed to write media to client: {}", err);
            self.rtc.disconnect();
        }
    }

    /// Requests a keyframe from the remote publisher.
    pub fn request_keyframe(&mut self, req: KeyframeRequest) {
        if let Some(mut writer) = self.rtc.writer(req.mid) {
            tracing::debug!(?req, "Requesting keyframe from client");
            if let Err(err) = writer.request_keyframe(req.rid, req.kind) {
                tracing::warn!("Failed to request keyframe from publisher: {err}");
            }
        }
    }
}
