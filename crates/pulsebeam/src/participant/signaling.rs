use pulsebeam_rtc::{DataChannelConfig, DataMessage, DataReliability, RtcConnection};
use pulsebeam_runtime::time::Instant;

use crate::media::ChannelId;

#[derive(Debug)]
pub(crate) struct SignalingChannel {
    channel: Option<ChannelId>,
    label: Box<str>,
    protocol: Box<str>,
    ordered: bool,
    reliability: DataReliability,
    negotiated: bool,
}

impl SignalingChannel {
    pub(crate) fn compile(config: &DataChannelConfig) -> Self {
        Self {
            channel: config.id,
            label: config.label.as_ref().into(),
            protocol: config.protocol.as_ref().into(),
            ordered: config.ordered,
            reliability: config.reliability,
            negotiated: config.negotiated,
        }
    }

    pub(crate) fn designates(&self, channel: ChannelId) -> bool {
        self.channel == Some(channel)
    }

    pub(crate) fn matches(&self, channel: ChannelId, config: &DataChannelConfig) -> bool {
        // Priority is deliberately not part of signaling-channel designation.
        self.designates(channel)
            && config.id == self.channel
            && config.label.as_ref() == self.label.as_ref()
            && config.protocol.as_ref() == self.protocol.as_ref()
            && config.ordered == self.ordered
            && config.reliability == self.reliability
            && config.negotiated == self.negotiated
    }
}

pub(crate) struct SignalingOutput {
    pub(crate) sequence: u64,
    pub(crate) cid: ChannelId,
    pub(crate) bytes: Vec<u8>,
    pub(crate) deadline: Instant,
}

#[derive(Clone, Copy)]
pub(crate) struct SignalingProgress {
    pub(crate) sequence: u64,
    pub(crate) cid: ChannelId,
    pub(crate) at: Instant,
}

pub(crate) enum SignalingWrite {
    Idle,
    Blocked,
    Admitted(SignalingProgress),
    Expired,
}

struct Pending {
    sequence: u64,
    cid: ChannelId,
    message: DataMessage,
    deadline: Instant,
}

#[derive(Default)]
pub(crate) struct SignalingWriter {
    pending: Option<Pending>,
}

impl SignalingWriter {
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn offer(&mut self, output: SignalingOutput) -> Result<(), SignalingOutput> {
        if self.pending.is_some() {
            return Err(output);
        }
        self.pending = Some(Pending {
            sequence: output.sequence,
            cid: output.cid,
            message: DataMessage::Binary(output.bytes.into()),
            deadline: output.deadline,
        });
        Ok(())
    }

    pub(crate) fn poll(&mut self, rtc: &mut RtcConnection, at: Instant) -> SignalingWrite {
        let Some(Pending {
            sequence,
            cid,
            message,
            deadline,
        }) = self.pending.take()
        else {
            return SignalingWrite::Idle;
        };
        if at >= deadline {
            metrics::counter!("native_signaling_handoff_failure", "lane" => "output_deadline")
                .increment(1);
            return SignalingWrite::Expired;
        }
        match rtc.send_data(at.into(), cid, message) {
            Ok(()) => SignalingWrite::Admitted(SignalingProgress { sequence, cid, at }),
            Err(rejected) => {
                metrics::counter!("native_signaling_handoff_pressure", "lane" => "rtc_output")
                    .increment(1);
                self.pending = Some(Pending {
                    sequence,
                    cid,
                    message: rejected.message,
                    deadline,
                });
                SignalingWrite::Blocked
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_designation_uses_compiled_fields_without_constraining_priority() {
        use crate::track::DataTrackIntent;
        use pulsebeam_rtc::DataChannelPriority;

        let zero = ChannelId::from_value(0);
        let other = ChannelId::from_value(2);
        let expected = DataTrackIntent::native_signaling_config();
        let channel = SignalingChannel::compile(&expected);
        assert!(channel.designates(zero));
        assert!(!channel.designates(other));
        assert!(channel.matches(zero, &expected));
        assert!(!channel.matches(other, &expected));
        assert!(channel.matches(
            zero,
            &DataChannelConfig {
                priority: DataChannelPriority::HIGH,
                ..expected.clone()
            }
        ));
        for invalid in [
            DataChannelConfig {
                id: Some(other),
                ..expected.clone()
            },
            DataChannelConfig {
                id: None,
                ..expected.clone()
            },
            DataChannelConfig {
                negotiated: false,
                ..expected.clone()
            },
            DataChannelConfig {
                ordered: false,
                ..expected.clone()
            },
            DataChannelConfig {
                reliability: DataReliability::MaxRetransmits(0),
                ..expected.clone()
            },
            DataChannelConfig {
                label: "v1/sys/signaling/extra".into(),
                ..expected.clone()
            },
            DataChannelConfig {
                protocol: "other".into(),
                ..expected
            },
        ] {
            assert!(!channel.matches(zero, &invalid));
        }
    }

    #[test]
    fn single_slot_handoff_preserves_rejected_bytes_and_terminal_cleanup() {
        let mut writer = SignalingWriter::default();
        let output = |sequence| SignalingOutput {
            sequence,
            cid: ChannelId::from_value(0),
            bytes: vec![42; 32],
            deadline: Instant::now(),
        };
        let first = output(1);
        let pointer = first.bytes.as_ptr();
        assert!(writer.offer(first).is_ok());
        let Some(Pending {
            message: DataMessage::Binary(bytes),
            ..
        }) = writer.pending.as_ref()
        else {
            panic!("opaque binary output");
        };
        assert_eq!(bytes.as_ptr(), pointer);
        let second = output(2);
        let pointer = second.bytes.as_ptr();
        let deadline = second.deadline;
        let second = writer.offer(second).err().expect("one pending batch");
        assert_eq!(second.sequence, 2);
        assert_eq!(second.bytes.as_ptr(), pointer);
        assert_eq!(second.deadline, deadline);
        writer.clear();
        assert!(!writer.has_pending());
        assert!(writer.offer(second).is_ok());
        writer.clear();
        assert!(!writer.has_pending());
    }
}
