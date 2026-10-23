//! Opt-in, bounded native RTP header observations for controlled diagnostics.
//! No payloads, protocol parser, actor reentry or receiver oracle lives here.
use pulsebeam_webrtc_sys as rtc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) const TARGET: &str = "pulsebeam_agent_native::native_rtp";
const PACKET_BUDGET: usize = 2048;

pub(crate) struct Factory(tracing::Span);

impl Factory {
    pub(crate) fn new() -> Self {
        Self(tracing::Span::current())
    }
}

impl rtc::RtcEventLogFactory for Factory {
    fn create(&self, _: rtc::EventLogEnvironment) -> Box<dyn rtc::RtcEventLogger> {
        Box::new(Logger {
            span: self.0.clone(),
            remaining: AtomicUsize::new(PACKET_BUDGET),
        })
    }
}

struct Logger {
    span: tracing::Span,
    remaining: AtomicUsize,
}

impl rtc::RtcEventLogger for Logger {
    fn start(&self, _: rtc::EventLogOutput, _: rtc::EventLogOutputPeriod) -> bool {
        // This observer consumes native event callbacks, not an output stream.
        false
    }

    fn stop(&self) {}

    fn log(&self, event: rtc::RtcEvent) {
        if event.kind() != rtc::RtcEventKind::RtpPacketIncoming
            || self
                .remaining
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(1)
                })
                .is_err()
        {
            return;
        }
        let rtc::RtcEventData::RtpPacketIncoming(packet) = event.data() else {
            return;
        };
        let dd = event.rtp_extension(rtc::RtcEventRtpExtensionKind::DependencyDescriptorMandatory);
        let _entered = self.span.enter();
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
