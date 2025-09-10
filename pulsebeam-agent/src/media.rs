use anyhow::Result;
use std::time::Duration;
use str0m::media::{MediaKind, Mid};
use tokio::sync::mpsc;

/// Media input for sending to remote peers
#[derive(Debug, Clone)]
pub struct MediaInput {
    pub mid: Mid,
    pub payload_type: u8,
    pub timestamp: u32,
    pub data: Vec<u8>,
    pub marker: bool,
}

/// Media source trait for pluggable media inputs
pub trait MediaSource: Send + Sync {
    /// Get the next media frame
    fn next_frame(&mut self) -> impl Future<Output = Option<MediaInput>>;

    /// Get the media kind this source produces
    fn media_kind(&self) -> MediaKind;

    /// Get the preferred payload type
    fn payload_type(&self) -> u8;

    /// Stop the media source
    async fn stop(&mut self) {}
}

/// File-based media source for testing and recordings
pub struct FileMediaSource {
    file_path: String,
    media_kind: MediaKind,
    payload_type: u8,
    frame_rate: u32,
    last_timestamp: u32,
    running: bool,
}

impl FileMediaSource {
    pub fn new(
        file_path: String,
        media_kind: MediaKind,
        payload_type: u8,
        frame_rate: u32,
    ) -> Self {
        Self {
            file_path,
            media_kind,
            payload_type,
            frame_rate,
            last_timestamp: 0,
            running: true,
        }
    }

    pub async fn from_file(path: &str, media_kind: MediaKind) -> Result<Self> {
        let payload_type = match media_kind {
            MediaKind::Audio => 111, // Opus
            MediaKind::Video => 96,  // H.264
        };

        let frame_rate = match media_kind {
            MediaKind::Audio => 50, // 20ms frames
            MediaKind::Video => 30, // 30fps
        };

        Ok(Self::new(
            path.to_string(),
            media_kind,
            payload_type,
            frame_rate,
        ))
    }
}

impl MediaSource for FileMediaSource {
    async fn next_frame(&mut self) -> Option<MediaInput> {
        if !self.running {
            return None;
        }

        // TODO: Implement actual file reading logic based on file format
        // This would parse media containers like MP4, WebM, etc.
        let frame_duration = Duration::from_millis(1000 / self.frame_rate as u64);
        tokio::time::sleep(frame_duration).await;

        let timestamp_increment = match self.media_kind {
            MediaKind::Audio => 960,  // 48kHz, 20ms frames
            MediaKind::Video => 3000, // 90kHz, ~33ms frames for 30fps
        };

        self.last_timestamp += timestamp_increment;

        // Placeholder: Read actual file data here
        let frame_size = match self.media_kind {
            MediaKind::Audio => 160,  // Small audio frame
            MediaKind::Video => 1500, // Video frame
        };

        Some(MediaInput {
            mid: Mid::from("0"), // Will be updated by the handle
            payload_type: self.payload_type,
            timestamp: self.last_timestamp,
            data: vec![0; frame_size], // Replace with actual file data
            marker: self.media_kind == MediaKind::Video,
        })
    }

    fn media_kind(&self) -> MediaKind {
        self.media_kind
    }

    fn payload_type(&self) -> u8 {
        self.payload_type
    }

    async fn stop(&mut self) {
        self.running = false;
    }
}

/// Test pattern media source for benchmarking and simulation
pub struct TestPatternSource {
    media_kind: MediaKind,
    payload_type: u8,
    frame_rate: u32,
    frame_size: usize,
    timestamp: u32,
    sequence: u16,
    running: bool,
    pattern_type: TestPatternType,
}

#[derive(Clone)]
pub enum TestPatternType {
    Incremental,
    Random,
    Sine,
    Static(u8),
}

impl TestPatternSource {
    pub fn new(
        media_kind: MediaKind,
        payload_type: u8,
        frame_rate: u32,
        frame_size: usize,
    ) -> Self {
        Self {
            media_kind,
            payload_type,
            frame_rate,
            frame_size,
            timestamp: 0,
            sequence: 0,
            running: true,
            pattern_type: TestPatternType::Incremental,
        }
    }

    pub fn with_pattern(mut self, pattern: TestPatternType) -> Self {
        self.pattern_type = pattern;
        self
    }

    pub fn stop(&mut self) {
        self.running = false;
    }
}

impl MediaSource for TestPatternSource {
    async fn next_frame(&mut self) -> Option<MediaInput> {
        if !self.running {
            return None;
        }

        let frame_interval = Duration::from_millis(1000 / self.frame_rate as u64);
        tokio::time::sleep(frame_interval).await;

        let timestamp_increment = match self.media_kind {
            MediaKind::Audio => 960,  // 48kHz, 20ms frames
            MediaKind::Video => 3000, // 90kHz, ~33ms frames for 30fps
        };

        self.timestamp += timestamp_increment;
        self.sequence += 1;

        // Generate test pattern data
        let mut data = vec![0u8; self.frame_size];
        match &self.pattern_type {
            TestPatternType::Incremental => {
                for (i, byte) in data.iter_mut().enumerate() {
                    *byte = ((i + self.sequence as usize) % 256) as u8;
                }
            }
            TestPatternType::Random => {
                for byte in data.iter_mut() {
                    *byte = (self.sequence as u8).wrapping_mul(17).wrapping_add(42);
                }
            }
            TestPatternType::Sine => {
                let freq = 440.0; // A4 note for audio, or visual pattern for video
                for (i, byte) in data.iter_mut().enumerate() {
                    let sample = ((i as f64 * freq * 2.0 * std::f64::consts::PI / 48000.0).sin()
                        * 127.0) as i8;
                    *byte = sample as u8;
                }
            }
            TestPatternType::Static(value) => {
                data.fill(*value);
            }
        }

        Some(MediaInput {
            mid: Mid::from("0"), // Will be updated by the handle
            payload_type: self.payload_type,
            timestamp: self.timestamp,
            data,
            marker: self.media_kind == MediaKind::Video,
        })
    }

    fn media_kind(&self) -> MediaKind {
        self.media_kind
    }

    fn payload_type(&self) -> u8 {
        self.payload_type
    }

    async fn stop(&mut self) {
        self.running = false;
    }
}

/// External RTP source for SIP integration or RTMP ingestion
pub struct ExternalRtpSource {
    media_kind: MediaKind,
    payload_type: u8,
    packet_rx: mpsc::Receiver<(u32, Vec<u8>)>, // (timestamp, data)
}

impl ExternalRtpSource {
    pub fn new(media_kind: MediaKind, payload_type: u8) -> (Self, mpsc::Sender<(u32, Vec<u8>)>) {
        let (packet_tx, packet_rx) = mpsc::channel(1000);
        let source = Self {
            media_kind,
            payload_type,
            packet_rx,
        };
        (source, packet_tx)
    }
}

impl MediaSource for ExternalRtpSource {
    async fn next_frame(&mut self) -> Option<MediaInput> {
        if let Some((timestamp, data)) = self.packet_rx.recv().await {
            Some(MediaInput {
                mid: Mid::from("0"), // Will be updated by the handle
                payload_type: self.payload_type,
                timestamp,
                data,
                marker: self.media_kind == MediaKind::Video,
            })
        } else {
            None
        }
    }

    fn media_kind(&self) -> MediaKind {
        self.media_kind
    }

    fn payload_type(&self) -> u8 {
        self.payload_type
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_test_pattern_source() {
        let mut source = TestPatternSource::new(MediaKind::Video, 96, 30, 1000);

        let frame = timeout(Duration::from_secs(1), source.next_frame()).await;
        assert!(frame.is_ok());
        let frame = frame.unwrap();
        assert!(frame.is_some());

        let media_input = frame.unwrap();
        assert_eq!(media_input.payload_type, 96);
        assert_eq!(media_input.data.len(), 1000);
        assert!(media_input.marker); // Video frames should have marker
    }

    #[tokio::test]
    async fn test_external_rtp_source() {
        let (mut source, tx) = ExternalRtpSource::new(MediaKind::Audio, 111);

        // Send a test packet
        let test_data = vec![1, 2, 3, 4];
        let timestamp = 1000;
        tx.send((timestamp, test_data.clone())).await.unwrap();

        // Receive the packet
        let frame = timeout(Duration::from_millis(100), source.next_frame()).await;
        assert!(frame.is_ok());
        let frame = frame.unwrap();
        assert!(frame.is_some());

        let media_input = frame.unwrap();
        assert_eq!(media_input.timestamp, timestamp);
        assert_eq!(media_input.data, test_data);
    }

    #[tokio::test]
    async fn test_file_media_source() {
        let mut source = FileMediaSource::new("test.mp4".to_string(), MediaKind::Video, 96, 30);

        let frame = timeout(Duration::from_secs(1), source.next_frame()).await;
        assert!(frame.is_ok());
        assert!(frame.unwrap().is_some());
    }
}
