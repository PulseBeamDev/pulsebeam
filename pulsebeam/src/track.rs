use std::sync::Arc;

use async_stream::stream;
use futures::Stream;
use futures_concurrency::stream::Merge;
use str0m::{media::Rid, rtp::RtpPacket};
use tokio::sync::watch;

use pulsebeam_runtime::sync::spmc;

#[derive(Debug)]
pub struct KeyframeRequest {
    pub request: str0m::media::KeyframeRequest,
    pub requested_at: tokio::time::Instant,
}

/// Metadata for a track — wraps `TrackMeta` in the app layer.
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct TrackMeta {
    pub id: Arc<crate::entity::TrackId>,
    pub kind: str0m::media::MediaKind,
    pub simulcast_rids: Option<Vec<Rid>>,
}

/// Simulcast receiver for one RID.
#[derive(Clone, Debug)]
pub struct SimulcastReceiver {
    pub rid: Option<Rid>,
    pub channel: spmc::Receiver<RtpPacket>,
    /// Used to request a keyframe from the sender.
    pub keyframe_requester: watch::Sender<Option<KeyframeRequest>>,
}

impl SimulcastReceiver {
    /// Returns an async stream of RTP packets for this RID.
    pub fn stream(mut self) -> impl Stream<Item = Arc<RtpPacket>> + Send + 'static {
        stream! {
            loop {
                match self.channel.recv().await {
                    Ok(pkt) => yield pkt,
                    Err(spmc::RecvError::Lagged(_)) => continue, // skip dropped frames
                    Err(spmc::RecvError::Closed) => break,
                }
            }
        }
    }
}

/// Simulcast sender for one RID.
#[derive(Debug)]
pub struct SimulcastSender {
    pub rid: Option<Rid>,
    pub channel: spmc::Sender<RtpPacket>,
    /// Used to receive keyframe requests from the receiver.
    pub keyframe_requests: watch::Receiver<Option<KeyframeRequest>>,
}

/// Sender half of a track (typically owned by publisher/participant)
pub struct TrackSender {
    pub meta: Arc<TrackMeta>,
    pub simulcast: Vec<SimulcastSender>,
}

impl TrackSender {
    pub fn send(&self, rid: Option<&Rid>, pkt: RtpPacket) {
        if let Some(sender) = self
            .simulcast
            .iter()
            .find(|s| s.rid.as_ref() == rid)
            .or_else(|| self.simulcast.first())
        {
            tracing::trace!(?self.meta, ?pkt, ?rid, ?sender, "sent to track");
            sender.channel.send(pkt);
        } else {
            panic!("expected sender to always available");
        }
    }
}

/// Receiver half of a track (typically consumed by subscribers)
#[derive(Clone, Debug)]
pub struct TrackReceiver {
    pub meta: Arc<TrackMeta>,
    pub simulcast: Vec<SimulcastReceiver>,
}

impl TrackReceiver {
    /// Merge all RID layers into a single unified stream.
    pub fn merged_stream(&self) -> impl Stream<Item = Arc<RtpPacket>> + Send + 'static {
        let streams: Vec<_> = self.simulcast.iter().cloned().map(|r| r.stream()).collect();
        streams.merge()
    }

    /// Convenience for accessing a specific RID
    pub fn by_rid(&self, rid: Option<&Rid>) -> Option<SimulcastReceiver> {
        self.simulcast
            .iter()
            .find(|s| s.rid.as_ref() == rid)
            .cloned()
    }

    pub fn by_default(&mut self) -> Option<&mut SimulcastReceiver> {
        for simulcast in &mut self.simulcast {
            let Some(rid) = simulcast.rid else {
                return Some(simulcast);
            };

            if rid.starts_with('f') {
                return Some(simulcast);
            }
        }
        None
    }

    /// Request a keyframe on all simulcast layers.
    pub fn request_keyframe(&self, rid: Option<&Rid>) {
        let Some(receiver) = self.by_rid(rid) else {
            tracing::warn!("no receiver found for a keyframe request");
            return;
        };

        let request = str0m::media::KeyframeRequest {
            mid: self.meta.id.origin_mid,
            rid: receiver.rid,
            kind: str0m::media::KeyframeRequestKind::Pli,
        };
        let wrapped = KeyframeRequest {
            request,
            requested_at: tokio::time::Instant::now(),
        };
        let Ok(_) = receiver.keyframe_requester.send(Some(wrapped)) else {
            tracing::warn!(?request, "feedback channel is unavailable");
            return;
        };
    }
}

/// Construct a new Track (returns sender + receiver).
pub fn new(meta: Arc<TrackMeta>, capacity: usize) -> (TrackSender, TrackReceiver) {
    let simulcast_rids = if let Some(rids) = &meta.simulcast_rids {
        rids.iter().map(|rid| Some(*rid)).collect()
    } else {
        vec![None]
    };

    let mut senders = Vec::new();
    let mut receivers = Vec::new();

    for rid in simulcast_rids {
        let (tx, rx) = spmc::channel(capacity);
        let (keyframe_tx, keyframe_rx) = watch::channel(None);

        senders.push(SimulcastSender {
            rid,
            channel: tx,
            keyframe_requests: keyframe_rx,
        });

        receivers.push(SimulcastReceiver {
            rid,
            channel: rx,
            keyframe_requester: keyframe_tx,
        });
    }

    (
        TrackSender {
            meta: meta.clone(),
            simulcast: senders,
        },
        TrackReceiver {
            meta,
            simulcast: receivers,
        },
    )
}
