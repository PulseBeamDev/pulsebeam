//! Opt-in, bounded native header and congestion-control event observations.
//! No media payloads, protocol parser, actor reentry or receiver oracle lives here.
use pulsebeam_webrtc_sys as rtc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) const TARGET: &str = "pulsebeam_agent_native::native_rtp";
pub(crate) const CC_TARGET: &str = "pulsebeam_agent_native::native_cc";
const PACKET_BUDGET: usize = 2048;

pub(crate) struct Factory(tracing::Span);

impl Factory {
    pub(crate) fn new() -> Self {
        Self(tracing::Span::current())
    }
}

impl rtc::RtcEventLogFactory for Factory {
    fn create(&self, environment: rtc::EventLogEnvironment) -> Box<dyn rtc::RtcEventLogger> {
        let span = if tracing::enabled!(target: CC_TARGET, tracing::Level::TRACE) {
            tracing::trace_span!(target: CC_TARGET, parent: &self.0, "native_event_log")
        } else {
            self.0.clone()
        };
        Box::new(Logger {
            span,
            environment,
            remaining: AtomicUsize::new(PACKET_BUDGET),
        })
    }
}

struct Logger {
    span: tracing::Span,
    environment: rtc::EventLogEnvironment,
    remaining: AtomicUsize,
}

impl rtc::RtcEventLogger for Logger {
    fn start(&self, _: rtc::EventLogOutput, _: rtc::EventLogOutputPeriod) -> bool {
        // This observer consumes native event callbacks, not an output stream.
        false
    }

    fn stop(&self) {}

    fn log(&self, event: rtc::RtcEvent) {
        let congestion_event = matches!(
            event.kind(),
            rtc::RtcEventKind::AlrState
                | rtc::RtcEventKind::RouteChange
                | rtc::RtcEventKind::AudioSendStreamConfig
                | rtc::RtcEventKind::VideoSendStreamConfig
                | rtc::RtcEventKind::BweUpdateDelayBased
                | rtc::RtcEventKind::BweUpdateLossBased
                | rtc::RtcEventKind::ProbeClusterCreated
                | rtc::RtcEventKind::ProbeResultFailure
                | rtc::RtcEventKind::ProbeResultSuccess
                | rtc::RtcEventKind::RtcpPacketIncoming
                | rtc::RtcEventKind::RtcpPacketOutgoing
                | rtc::RtcEventKind::RtpPacketOutgoing
        );
        let enabled = if congestion_event {
            tracing::enabled!(target: CC_TARGET, tracing::Level::TRACE)
        } else {
            event.kind() == rtc::RtcEventKind::RtpPacketIncoming
                && tracing::enabled!(target: TARGET, tracing::Level::TRACE)
        };
        if !enabled
            || self
                .remaining
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(1)
                })
                .is_err()
        {
            return;
        }
        let _entered = self.span.enter();
        if congestion_event {
            tracing::trace!(
                target: CC_TARGET,
                observer = ?self.span.id(),
                native_time = ?self.environment.timestamp(),
                event_time_us = event.timestamp_us(),
                kind = ?event.kind(),
                data = ?event.data(),
                "Native congestion-control observation"
            );
            return;
        }
        let rtc::RtcEventData::RtpPacketIncoming(packet) = event.data() else {
            return;
        };
        let dd = event.rtp_extension(rtc::RtcEventRtpExtensionKind::DependencyDescriptorMandatory);
        tracing::trace!(
            target: TARGET,
            event_time_us = event.timestamp_us(),
            ssrc = packet.ssrc,
            pt = packet.payload_type,
            seq = packet.sequence_number,
            rtp_ts = packet.timestamp,
            marker = packet.marker,
            len = packet.payload_length,
            rtx_original_seq = ?packet.rtx_original_sequence_number,
            ?dd,
            "Native received RTP packet"
        );
    }
}
