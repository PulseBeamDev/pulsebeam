use std::sync::Arc;

use pulsebeam_runtime::sync::spmc;
use str0m::{media::Rid, rtp::RtpPacket};

use crate::message::TrackMeta;

/// A single simulcast layer receiver (identified by `rid`, if present).
#[derive(Clone, Debug)]
pub struct SimulcastReceiver {
    pub rid: Option<Rid>,
    pub channel: spmc::Receiver<Arc<RtpPacket>>,
}

/// A single simulcast layer sender (identified by `rid`, if present).
#[derive(Debug)]
pub struct SimulcastSender {
    pub rid: Option<Rid>,
    pub channel: spmc::Sender<Arc<RtpPacket>>,
}

/// Receiver for an entire track (may have multiple simulcast layers).
#[derive(Clone, Debug)]
pub struct TrackReceiver {
    pub meta: Arc<TrackMeta>,
    pub simulcast: Arc<Vec<SimulcastReceiver>>,
}

impl TrackReceiver {
    /// Get a receiver for a specific RID, if present.
    pub fn by_rid(&self, rid: Option<&Rid>) -> Option<&SimulcastReceiver> {
        self.simulcast.iter().find(|r| match (&r.rid, rid) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        })
    }
}

/// Sender for an entire track (may have multiple simulcast layers).
pub struct TrackSender {
    pub meta: Arc<TrackMeta>,
    pub simulcast: Arc<Vec<SimulcastSender>>,
}

impl TrackSender {
    /// Send to all layers (e.g. for non-simulcast).
    pub fn send_all(&self, packet: Arc<RtpPacket>) {
        for tx in self.simulcast.iter() {
            tx.channel.send(packet.clone());
        }
    }

    /// Send only to a specific RID (for simulcast case).
    pub fn send_rid(&self, rid: &Rid, packet: Arc<RtpPacket>) {
        if let Some(tx) = self.simulcast.iter().find(|s| s.rid.as_ref() == Some(rid)) {
            tx.channel.send(packet);
        }
    }

    /// Get a specific layer sender.
    pub fn by_rid(&self, rid: Option<&Rid>) -> Option<&SimulcastSender> {
        self.simulcast.iter().find(|s| match (&s.rid, rid) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        })
    }
}

/// Create a new (TrackSender, TrackReceiver) pair.
pub fn new(meta: Arc<TrackMeta>, cap: usize) -> (TrackSender, TrackReceiver) {
    let simulcast_pairs: Vec<(SimulcastSender, SimulcastReceiver)> =
        if let Some(rids) = &meta.simulcast_rids {
            rids.iter()
                .map(|rid| {
                    let (tx, rx) = spmc::channel(cap);
                    (
                        SimulcastSender {
                            rid: Some(rid.clone()),
                            channel: tx,
                        },
                        SimulcastReceiver {
                            rid: Some(rid.clone()),
                            channel: rx,
                        },
                    )
                })
                .collect()
        } else {
            let (tx, rx) = spmc::channel(cap);
            vec![(
                SimulcastSender {
                    rid: None,
                    channel: tx,
                },
                SimulcastReceiver {
                    rid: None,
                    channel: rx,
                },
            )]
        };

    let (senders, receivers): (Vec<_>, Vec<_>) = simulcast_pairs.into_iter().unzip();

    (
        TrackSender {
            meta: meta.clone(),
            simulcast: Arc::new(senders),
        },
        TrackReceiver {
            meta,
            simulcast: Arc::new(receivers),
        },
    )
}

/// Pick a "pinned" RID (for example, prefer 'f' for full resolution).
pub fn pin_rid(simulcast_rids: &Option<Vec<Rid>>) -> Option<Rid> {
    simulcast_rids
        .as_ref()?
        .iter()
        .find(|rid| rid.starts_with('f'))
        .cloned()
}
