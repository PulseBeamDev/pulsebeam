use std::collections::{BTreeMap, VecDeque};

use pulsebeam_agent_core::{MonotonicTime, TrackId};
use tokio::sync::mpsc;

use crate::media::RtpPacket;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaFrame {
    pub track_id: TrackId,
    pub timestamp: u32,
    pub capture_time: MonotonicTime,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

pub struct FrameSender {
    sender: mpsc::Sender<MediaFrame>,
}

pub struct FrameReceiver {
    receiver: mpsc::Receiver<MediaFrame>,
}

pub fn frame_channel(capacity: usize) -> (FrameSender, FrameReceiver) {
    debug_assert!(capacity > 0);
    let (sender, receiver) = mpsc::channel(capacity);
    (FrameSender { sender }, FrameReceiver { receiver })
}

impl FrameSender {
    pub async fn send(&self, frame: MediaFrame) -> Result<(), PipelineError> {
        self.sender
            .send(frame)
            .await
            .map_err(|_| PipelineError::Closed)
    }
}

impl FrameReceiver {
    pub async fn recv(&mut self) -> Result<MediaFrame, PipelineError> {
        self.receiver.recv().await.ok_or(PipelineError::Closed)
    }
}

pub struct JitterBuffer {
    capacity: usize,
    frames: BTreeMap<(MonotonicTime, u32), MediaFrame>,
}

impl JitterBuffer {
    pub fn new(capacity: usize) -> Self {
        debug_assert!(capacity > 0);
        Self {
            capacity,
            frames: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, frame: MediaFrame) {
        if self.frames.len() == self.capacity {
            self.frames.pop_first();
        }
        self.frames
            .insert((frame.capture_time, frame.timestamp), frame);
    }

    pub fn pop_ready(&mut self, now: MonotonicTime) -> Option<MediaFrame> {
        let key = self.frames.keys().next().copied()?;
        if key.0 > now {
            return None;
        }
        self.frames.remove(&key)
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new(128)
    }
}

pub struct MediaPipeline {
    mtu: usize,
    next_sequence: u16,
    assemblies: BTreeMap<u32, Vec<u8>>,
    pending: VecDeque<MediaFrame>,
}

impl MediaPipeline {
    pub fn new(mtu: usize) -> Result<Self, PipelineError> {
        if mtu == 0 {
            return Err(PipelineError::InvalidMtu);
        }
        Ok(Self {
            mtu,
            next_sequence: 0,
            assemblies: BTreeMap::new(),
            pending: VecDeque::new(),
        })
    }

    pub fn packetize(&mut self, frame: &MediaFrame) -> Vec<RtpPacket> {
        debug_assert!(self.mtu > 0);
        if frame.data.is_empty() {
            return vec![self.packet(frame, Vec::new(), true)];
        }
        let mut packets = Vec::new();
        let mut offset = 0usize;
        while offset < frame.data.len() {
            let end = offset.saturating_add(self.mtu).min(frame.data.len());
            let Some(chunk) = frame.data.get(offset..end) else {
                debug_assert!(false, "packetization slice must remain in bounds");
                break;
            };
            packets.push(self.packet(frame, chunk.to_vec(), end == frame.data.len()));
            offset = end;
        }
        packets
    }

    pub fn ingest(&mut self, packet: RtpPacket, track_id: TrackId, now: MonotonicTime) {
        let data = self.assemblies.entry(packet.timestamp).or_default();
        data.extend_from_slice(&packet.payload);
        if packet.marker
            && let Some(data) = self.assemblies.remove(&packet.timestamp)
        {
            self.pending.push_back(MediaFrame {
                track_id,
                timestamp: packet.timestamp,
                capture_time: now,
                keyframe: packet.marker,
                data,
            });
        }
        while self.assemblies.len() > 128 {
            let Some(oldest) = self.assemblies.keys().next().copied() else {
                break;
            };
            self.assemblies.remove(&oldest);
        }
    }

    pub fn poll_frame(&mut self) -> Option<MediaFrame> {
        self.pending.pop_front()
    }

    fn packet(&mut self, frame: &MediaFrame, payload: Vec<u8>, marker: bool) -> RtpPacket {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        RtpPacket {
            mid: frame.track_id.as_str().to_owned(),
            sequence,
            timestamp: frame.timestamp,
            marker,
            payload,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PipelineError {
    Closed,
    InvalidMtu,
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => formatter.write_str("media pipeline closed"),
            Self::InvalidMtu => formatter.write_str("media MTU must be non-zero"),
        }
    }
}

impl std::error::Error for PipelineError {}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn frame() -> MediaFrame {
        MediaFrame {
            track_id: TrackId::from("track"),
            timestamp: 4,
            capture_time: MonotonicTime::from_millis(10),
            keyframe: true,
            data: vec![1, 2, 3, 4, 5],
        }
    }

    #[test]
    fn packetization_and_reassembly_preserve_payload() {
        let mut pipeline = MediaPipeline::new(2).unwrap();
        let frame = frame();
        let packets = pipeline.packetize(&frame);
        for packet in packets {
            pipeline.ingest(packet, frame.track_id.clone(), frame.capture_time);
        }
        assert_eq!(pipeline.poll_frame().unwrap().data, frame.data);
    }

    #[test]
    fn jitter_buffer_is_time_ordered_and_bounded() {
        let mut jitter = JitterBuffer::new(2);
        let mut first = frame();
        first.capture_time = MonotonicTime::from_millis(20);
        jitter.insert(first);
        let mut second = frame();
        second.timestamp = 5;
        second.capture_time = MonotonicTime::from_millis(10);
        jitter.insert(second.clone());
        assert_eq!(
            jitter.pop_ready(MonotonicTime::from_millis(15)),
            Some(second)
        );
        assert_eq!(jitter.pop_ready(MonotonicTime::from_millis(15)), None);
    }
}
