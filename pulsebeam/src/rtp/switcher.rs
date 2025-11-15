use str0m::media::Frequency;

use crate::rtp::RtpPacket;
use crate::rtp::buffer::KeyframeBuffer;
use crate::rtp::timeline::Timeline;

#[derive(Debug)]
pub struct Switcher {
    /// The timeline for the currently active stream.
    timeline: Timeline,

    /// A high-priority slot for a packet from the *current* stream.
    /// This allows draining the last few packets of the old stream during a switch.
    pending: Option<RtpPacket>,

    /// The state for the *new* stream we are switching to.
    /// This is `Some` only when a switch is in progress.
    staging: Option<KeyframeBuffer>,
}

impl Switcher {
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            timeline: Timeline::new(clock_rate),
            pending: None,
            staging: None,
        }
    }

    /// Pushes a packet from the **old/current** stream.
    /// This is typically used to forward the stream that is already playing out.
    pub fn push(&mut self, pkt: RtpPacket) {
        // rewrite active stream here so we don't race with the new stream
        let pkt = self.timeline.rewrite(pkt);
        self.pending.replace(pkt);
    }

    /// Pushes a packet for the **new** stream we are preparing to switch to.
    /// The first call to this method will initiate the switching process.
    pub fn stage(&mut self, pkt: RtpPacket) {
        let staging = self.staging.get_or_insert_default();

        if pkt.is_keyframe_start {
            self.timeline.rebase(&pkt);
        }
        staging.push(pkt);
    }

    /// Returns true if the new stream has received a keyframe and is ready to be popped.
    pub fn is_ready(&self) -> bool {
        self.staging.as_ref().map(|s| s.is_ready()).unwrap_or(false)
    }

    /// Pops the next available packet, prioritizing the old stream to ensure a smooth drain.
    pub fn pop(&mut self) -> Option<RtpPacket> {
        // --- Priority 1: Drain the pending packet from the OLD stream. ---
        if let Some(pending_pkt) = self.pending.take() {
            return Some(pending_pkt);
        }

        // TODO: deal with older frames here.
        // --- Priority 2: Pop packets from the NEW stream if a switch is in progress. ---
        if let Some(staging) = &mut self.staging {
            if let Some(staged_pkt) = staging.pop() {
                return Some(self.timeline.rewrite(staged_pkt));
            } else {
                if staging.is_ready() {
                    self.staging = None;
                }
                return None;
            }
        }

        None
    }
}
