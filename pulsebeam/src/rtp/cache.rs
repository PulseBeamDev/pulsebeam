use std::collections::VecDeque;

use crate::rtp::RtpPacket;
use tokio::time::Instant;

const STREAM_CACHE_CAPACITY: usize = 512;

/// Per-stream ring buffer seeded from the last keyframe onward.
///
/// Shared across all subscribers of the same upstream stream. A new subscriber
/// can replay the cached segment for an instant switch instead of waiting for a
/// PLI round-trip.
#[derive(Debug)]
pub struct StreamCache {
    packets: VecDeque<RtpPacket>,
    has_keyframe: bool,
    current_keyframe_playout: Option<Instant>,
}

impl Default for StreamCache {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamCache {
    pub fn new() -> Self {
        Self {
            packets: VecDeque::new(),
            has_keyframe: false,
            current_keyframe_playout: None,
        }
    }

    pub fn push(&mut self, pkt: &RtpPacket) {
        if pkt.is_keyframe {
            let is_new_segment = self
                .current_keyframe_playout
                .map(|t| t != pkt.playout_time)
                .unwrap_or(true);
            if is_new_segment {
                self.packets.clear();
                self.has_keyframe = true;
                self.current_keyframe_playout = Some(pkt.playout_time);
            }
        }

        if self.has_keyframe {
            if self.packets.len() == STREAM_CACHE_CAPACITY {
                self.packets.pop_front();
            }
            self.packets.push_back(pkt.clone());
        }
    }

    pub fn has_keyframe(&self) -> bool {
        self.has_keyframe
    }

    /// Returns a cloned snapshot of cached packets starting from the keyframe.
    /// Returns `None` if no keyframe has been seen yet.
    pub fn replay(&self) -> Option<Vec<RtpPacket>> {
        if self.has_keyframe && !self.packets.is_empty() {
            Some(self.packets.iter().cloned().collect())
        } else {
            None
        }
    }

    pub fn clear(&mut self) {
        self.packets.clear();
        self.has_keyframe = false;
        self.current_keyframe_playout = None;
    }
}
