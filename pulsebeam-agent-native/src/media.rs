use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use pulsebeam_agent_core::{MediaKind, MonotonicTime, TrackId, TransportGeneration};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpPacket {
    pub mid: String,
    pub sequence: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaRoute {
    pub logical_mid: String,
    pub physical_mid: String,
    pub track_id: TrackId,
    pub kind: MediaKind,
    pub generation: TransportGeneration,
}

pub struct RtpRouter {
    routes: BTreeMap<String, MediaRoute>,
    held: BTreeMap<String, VecDeque<RtpPacket>>,
    hold_capacity: usize,
}

impl RtpRouter {
    pub fn new(hold_capacity: usize) -> Self {
        debug_assert!(hold_capacity > 0);
        Self {
            routes: BTreeMap::new(),
            held: BTreeMap::new(),
            hold_capacity,
        }
    }

    pub fn install(&mut self, route: MediaRoute) -> Vec<RtpPacket> {
        debug_assert!(!route.logical_mid.is_empty());
        debug_assert!(!route.physical_mid.is_empty());
        self.routes.insert(route.logical_mid.clone(), route.clone());
        self.held
            .remove(&route.logical_mid)
            .map(|packets| packets.into_iter().collect())
            .unwrap_or_default()
    }

    pub fn remove(&mut self, logical_mid: &str, generation: TransportGeneration) -> bool {
        let Some(route) = self.routes.get(logical_mid) else {
            return false;
        };
        if route.generation != generation {
            return false;
        }
        self.routes.remove(logical_mid).is_some()
    }

    pub fn route(&mut self, packet: RtpPacket) -> Result<Option<RtpPacket>, MediaError> {
        let Some(route) = self.routes.get(&packet.mid) else {
            let queue = self.held.entry(packet.mid.clone()).or_default();
            if queue.len() == self.hold_capacity {
                queue.pop_front();
            }
            queue.push_back(packet);
            return Ok(None);
        };
        let mut routed = packet;
        routed.mid.clone_from(&route.physical_mid);
        Ok(Some(routed))
    }

    pub fn route_for(&self, logical_mid: &str) -> Option<&MediaRoute> {
        self.routes.get(logical_mid)
    }
}

impl Default for RtpRouter {
    fn default() -> Self {
        Self::new(128)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryResult {
    pub packet: RtpPacket,
    pub missing: Vec<u16>,
}

pub struct RtpRecoveryBuffer {
    next_sequence: Option<u16>,
    capacity: usize,
    pending: BTreeMap<u16, RtpPacket>,
}

impl RtpRecoveryBuffer {
    pub fn new(capacity: usize) -> Self {
        debug_assert!(capacity > 0);
        Self {
            next_sequence: None,
            capacity,
            pending: BTreeMap::new(),
        }
    }

    pub fn accept(&mut self, packet: RtpPacket) -> Vec<RecoveryResult> {
        let Some(next) = self.next_sequence else {
            self.next_sequence = Some(packet.sequence.wrapping_add(1));
            return vec![RecoveryResult {
                packet,
                missing: Vec::new(),
            }];
        };
        if packet.sequence == next {
            self.next_sequence = Some(next.wrapping_add(1));
            return vec![RecoveryResult {
                packet,
                missing: Vec::new(),
            }];
        }
        let distance = packet.sequence.wrapping_sub(next);
        if distance < 0x8000 {
            if self.pending.len() == self.capacity {
                self.pending.pop_first();
            }
            let received = packet.clone();
            self.pending.insert(packet.sequence, packet);
            let missing = (0..distance)
                .map(|offset| next.wrapping_add(offset))
                .collect();
            return vec![RecoveryResult {
                packet: received,
                missing,
            }];
        }
        Vec::new()
    }

    pub fn recover(&mut self, packet: RtpPacket) -> Vec<RecoveryResult> {
        self.pending.insert(packet.sequence, packet);
        let mut output = Vec::new();
        while let Some(next) = self.next_sequence {
            let Some(packet) = self.pending.remove(&next) else {
                break;
            };
            self.next_sequence = Some(next.wrapping_add(1));
            output.push(RecoveryResult {
                packet,
                missing: Vec::new(),
            });
        }
        output
    }
}

impl Default for RtpRecoveryBuffer {
    fn default() -> Self {
        Self::new(128)
    }
}

pub struct KeyframeController {
    cooldown: Duration,
    last_request: BTreeMap<String, MonotonicTime>,
}

impl KeyframeController {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last_request: BTreeMap::new(),
        }
    }

    pub fn request(&mut self, mid: impl Into<String>, now: MonotonicTime) -> bool {
        let mid = mid.into();
        let allowed = self
            .last_request
            .get(&mid)
            .is_none_or(|last| now.duration_since(*last) >= self.cooldown);
        if allowed {
            self.last_request.insert(mid, now);
        }
        allowed
    }
}

impl Default for KeyframeController {
    fn default() -> Self {
        Self::new(Duration::from_secs(1))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BandwidthEstimate {
    pub bits_per_second: u64,
}

pub struct BandwidthController {
    bytes: u64,
    window_start: Option<MonotonicTime>,
    estimate: BandwidthEstimate,
}

impl BandwidthController {
    pub fn new(seed_bps: u64) -> Self {
        Self {
            bytes: 0,
            window_start: None,
            estimate: BandwidthEstimate {
                bits_per_second: seed_bps,
            },
        }
    }

    pub fn record(&mut self, bytes: usize, now: MonotonicTime) {
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        self.window_start.get_or_insert(now);
    }

    pub fn poll(&mut self, now: MonotonicTime) -> BandwidthEstimate {
        let Some(start) = self.window_start else {
            return self.estimate;
        };
        let elapsed_ms = now.duration_since(start).as_millis();
        if elapsed_ms < 200 {
            return self.estimate;
        }
        let elapsed_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX).max(1);
        let measured = self
            .bytes
            .saturating_mul(8)
            .saturating_mul(1000)
            .checked_div(elapsed_ms)
            .unwrap_or(0);
        self.estimate = BandwidthEstimate {
            bits_per_second: measured,
        };
        self.bytes = 0;
        self.window_start = Some(now);
        self.estimate
    }
}

impl Default for BandwidthController {
    fn default() -> Self {
        Self::new(500_000)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediaError {
    InvalidPacket,
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid media packet")
    }
}

impl std::error::Error for MediaError {}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn packet(mid: &str, sequence: u16) -> RtpPacket {
        RtpPacket {
            mid: mid.to_owned(),
            sequence,
            timestamp: u32::from(sequence),
            marker: true,
            payload: vec![1],
        }
    }

    #[test]
    fn logical_mid_maps_to_physical_mid_and_rejects_stale_remove() {
        let mut router = RtpRouter::default();
        let route = MediaRoute {
            logical_mid: "logical".to_owned(),
            physical_mid: "2".to_owned(),
            track_id: TrackId::from("track"),
            kind: MediaKind::Video,
            generation: TransportGeneration::new(4),
        };
        router.install(route);
        assert_eq!(
            router.route(packet("logical", 1)).unwrap().unwrap().mid,
            "2"
        );
        assert!(!router.remove("logical", TransportGeneration::new(3)));
        assert!(router.route_for("logical").is_some());
    }

    #[test]
    fn recovery_reports_gap_then_flushes_after_retransmission() {
        let mut recovery = RtpRecoveryBuffer::default();
        assert_eq!(
            recovery.accept(packet("0", 10))[0].missing,
            Vec::<u16>::new()
        );
        let gap = recovery.accept(packet("0", 12));
        assert_eq!(gap[0].missing, vec![11]);
        assert_eq!(gap[0].packet.sequence, 12);
        let recovered = recovery.recover(packet("0", 11));
        assert_eq!(
            recovered
                .iter()
                .map(|item| item.packet.sequence)
                .collect::<Vec<_>>(),
            vec![11, 12]
        );
    }

    #[test]
    fn bandwidth_estimate_uses_elapsed_monotonic_time() {
        let mut bandwidth = BandwidthController::new(1);
        bandwidth.record(1_000, MonotonicTime::ZERO);
        assert_eq!(
            bandwidth.poll(MonotonicTime::from_millis(200)),
            BandwidthEstimate {
                bits_per_second: 40_000
            }
        );
    }
}
