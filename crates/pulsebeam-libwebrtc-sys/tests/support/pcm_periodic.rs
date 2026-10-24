//! Native report/stats observations for the sustained PCM fixture.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct Report {
    pub peer: usize,
    pub incoming: bool,
    pub timestamp_us: i64,
    pub observed_at_us: i64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(super) struct Reports {
    pub created: Vec<(usize, i64)>,
    pub started: Vec<usize>,
    pub stopped: Vec<usize>,
    pub dropped: Vec<usize>,
    pub packets: Vec<Report>,
}

#[derive(Default)]
pub(super) struct Audit(Mutex<Reports>);

impl Audit {
    pub fn snapshot(&self) -> Reports {
        self.0.lock().unwrap().clone()
    }
}

pub(super) struct Factory {
    pub peer: usize,
    pub audit: Arc<Audit>,
}

struct Logger {
    peer: usize,
    environment: EventLogEnvironment,
    audit: Arc<Audit>,
    output: Mutex<Option<EventLogOutput>>,
}

impl RtcEventLogFactory for Factory {
    fn create(&self, environment: EventLogEnvironment) -> Box<dyn RtcEventLogger> {
        self.audit
            .0
            .lock()
            .unwrap()
            .created
            .push((self.peer, environment.timestamp().microseconds().unwrap()));
        Box::new(Logger {
            peer: self.peer,
            environment,
            audit: self.audit.clone(),
            output: Mutex::new(None),
        })
    }
}

pub(super) struct Output;
impl EventLogOutputSink for Output {
    fn is_active(&self) -> bool {
        true
    }
    fn write(&mut self, _: &[u8]) -> bool {
        true
    }
}

impl RtcEventLogger for Logger {
    fn start(&self, output: EventLogOutput, _: EventLogOutputPeriod) -> bool {
        let mut selected = self.output.lock().unwrap();
        if selected.is_some() {
            return false;
        }
        *selected = Some(output);
        self.audit.0.lock().unwrap().started.push(self.peer);
        true
    }
    fn stop(&self) {
        if let Some(mut output) = self.output.lock().unwrap().take() {
            output.flush();
            self.audit.0.lock().unwrap().stopped.push(self.peer);
        }
    }
    fn log(&self, event: RtcEvent) {
        let (incoming, bytes) = match event.data() {
            RtcEventData::RtcpPacketIncoming(bytes) => (true, bytes),
            RtcEventData::RtcpPacketOutgoing(bytes) => (false, bytes),
            _ => return,
        };
        if self.output.lock().unwrap().is_some() {
            self.audit.0.lock().unwrap().packets.push(Report {
                peer: self.peer,
                incoming,
                timestamp_us: event.timestamp_us(),
                // Native RtcEvent starts at zero; the SDK stock logger stamps
                // it on acceptance. Keep that input unchanged and observe this
                // caller logger's boundary using its exact creation environment.
                observed_at_us: self.environment.timestamp().microseconds().unwrap(),
                bytes,
            });
        }
    }
}
impl Drop for Logger {
    fn drop(&mut self) {
        self.stop();
        self.audit.0.lock().unwrap().dropped.push(self.peer);
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct Inbound {
    pub id: Vec<u8>,
    pub timestamp_us: i64,
    pub kind: Option<Vec<u8>>,
    pub ssrc: Option<u64>,
    pub packets_received: Option<u64>,
    pub packets_lost: Option<i64>,
    // Preserve the native floating-point value exactly, not rounded text.
    pub jitter_bits: Option<u64>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct Outbound {
    pub id: Vec<u8>,
    pub timestamp_us: i64,
    pub kind: Option<Vec<u8>>,
    pub ssrc: Option<u64>,
    pub packets_sent: Option<u64>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct Stats {
    pub peer: usize,
    pub timestamp_us: i64,
    pub inbound: Vec<Inbound>,
    pub outbound: Vec<Outbound>,
}

impl Stats {
    pub fn from_native(peer: usize, snapshot: PeerStatsSnapshot) -> Self {
        let mut result = Self {
            peer,
            timestamp_us: snapshot.timestamp_us,
            inbound: Vec::new(),
            outbound: Vec::new(),
        };
        for record in snapshot.records {
            match record {
                PeerStatsRecord::InboundRtp(v) => result.inbound.push(Inbound {
                    id: v.id,
                    timestamp_us: v.timestamp_us,
                    kind: v.kind,
                    ssrc: v.ssrc,
                    packets_received: v.packets_received,
                    packets_lost: v.packets_lost,
                    jitter_bits: v.jitter.map(f64::to_bits),
                }),
                PeerStatsRecord::OutboundRtp(v) => result.outbound.push(Outbound {
                    id: v.id,
                    timestamp_us: v.timestamp_us,
                    kind: v.kind,
                    ssrc: v.ssrc,
                    packets_sent: v.packets_sent,
                }),
                _ => {}
            }
        }
        result
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct Observation {
    pub media_started_at_us: u128,
    pub before: [Stats; 2],
    pub after: [Stats; 2],
    pub reports: Reports,
}

// Decode only native event payloads, never encrypted simulated-network bytes.
pub(super) fn packet_types(bytes: &[u8]) -> Vec<u8> {
    let mut remaining = bytes;
    let mut result = Vec::new();
    while !remaining.is_empty() {
        assert!(remaining.len() >= 4);
        assert_eq!(remaining[0] >> 6, 2);
        let length = (usize::from(u16::from_be_bytes([remaining[2], remaining[3]])) + 1) * 4;
        assert!(length <= remaining.len());
        result.push(remaining[1]);
        remaining = &remaining[length..];
    }
    result
}
