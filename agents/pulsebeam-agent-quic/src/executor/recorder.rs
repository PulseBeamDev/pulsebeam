//! Bounded reference storage for an ordinary pinned encoded subscriber.
//!
//! The application subscribes with the normal Agent intent and supplies units
//! from `LinuxAgent::next_h264` and `LinuxAgent::next_opus` to this sink. These
//! are synchronous filesystem operations: keep them outside Agent turns. The
//! sink does not mux, synchronize clocks, request retransmission, or promise
//! crash durability. Stop on storage failure; a partial tail may remain.

use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
};

use super::linux::{ReceivedH264, ReceivedOpus};

const INDEX: &str = "index.tsv";
const HEADER: &str =
    "file\tpublication_id\tclock_hz\trtp_timestamp\tfirst_seq\tlast_seq\tdiscontinuity\tbytes\n";

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub units: u32,
    /// Total logical bytes across the payloads and index.
    pub bytes: u64,
}

#[derive(Debug)]
pub enum RecordError {
    InvalidConfiguration,
    InvalidUnit,
    Limit,
    Storage(io::Error),
    Stopped,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Recorded {
    Stored,
    OtherPublication,
}

struct Source {
    id: String,
    last_sequence: Option<u16>,
}

pub struct Recorder {
    directory: PathBuf,
    index: File,
    limits: Limits,
    units: u32,
    bytes: u64,
    opus: Option<Source>,
    h264: Option<Source>,
    stopped: bool,
}

#[derive(Clone, Copy)]
enum Codec {
    Opus,
    H264,
}

impl Recorder {
    /// Use a new directory. The two optional IDs are the ordinary subscription
    /// pins, which remain fixed for the life of this recording.
    pub fn create(
        directory: &Path,
        opus: Option<String>,
        h264: Option<String>,
        limits: Limits,
    ) -> Result<Self, RecordError> {
        let bytes = HEADER.len() as u64;
        if limits.units == 0 || limits.bytes < bytes || (opus.is_none() && h264.is_none()) {
            return Err(RecordError::InvalidConfiguration);
        }
        let source = |id: String| {
            if id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Err(RecordError::InvalidConfiguration);
            }
            Ok(Source {
                id,
                last_sequence: None,
            })
        };
        let opus = opus.map(source).transpose()?;
        let h264 = h264.map(source).transpose()?;
        fs::create_dir(directory).map_err(RecordError::Storage)?;
        let mut index = File::create_new(directory.join(INDEX)).map_err(RecordError::Storage)?;
        index
            .write_all(HEADER.as_bytes())
            .map_err(RecordError::Storage)?;
        Ok(Self {
            directory: directory.into(),
            index,
            limits,
            units: 0,
            bytes,
            opus,
            h264,
            stopped: false,
        })
    }

    pub fn record_opus(&mut self, packet: &ReceivedOpus) -> Result<Recorded, RecordError> {
        self.record(
            Codec::Opus,
            &packet.publication_id,
            packet.timestamp,
            (packet.sequence, packet.sequence),
            &packet.payload,
        )
    }

    pub fn record_h264(&mut self, frame: &ReceivedH264) -> Result<Recorded, RecordError> {
        self.record(
            Codec::H264,
            &frame.publication_id,
            frame.timestamp,
            (frame.first_sequence, frame.last_sequence),
            &frame.access_unit,
        )
    }

    fn record(
        &mut self,
        codec: Codec,
        publication_id: &str,
        timestamp: u32,
        (first, last): (u16, u16),
        payload: &[u8],
    ) -> Result<Recorded, RecordError> {
        if self.stopped {
            return Err(RecordError::Stopped);
        }
        let (source, extension, clock, max_payload) = match codec {
            Codec::Opus => (
                &mut self.opus,
                "opus",
                48_000,
                pulsebeam_core::fabric::MAX_WAN_RTP_BYTES,
            ),
            Codec::H264 => (
                &mut self.h264,
                "h264",
                90_000,
                pulsebeam_core::h264::MAX_ACCESS_UNIT_SIZE,
            ),
        };
        let Some(source) = source.as_mut().filter(|source| source.id == publication_id) else {
            return Ok(Recorded::OtherPublication);
        };
        if payload.is_empty() || payload.len() > max_payload {
            return Err(RecordError::InvalidUnit);
        }
        let discontinuity = u8::from(
            source
                .last_sequence
                .is_some_and(|previous| previous.wrapping_add(1) != first),
        );
        let filename = format!("{:010}.{extension}", self.units);
        let len = payload.len();
        let line = format!(
            "{filename}\t{publication_id}\t{clock}\t{timestamp}\t{first}\t{last}\t{discontinuity}\t{len}\n"
        );
        let used = self
            .bytes
            .saturating_add(len as u64)
            .saturating_add(line.len() as u64);
        if self.units >= self.limits.units || used > self.limits.bytes {
            self.stopped = true;
            return Err(RecordError::Limit);
        }
        let result = File::create_new(self.directory.join(filename))
            .and_then(|mut file| file.write_all(payload))
            .and_then(|()| self.index.write_all(line.as_bytes()));
        if let Err(error) = result {
            self.stopped = true;
            return Err(RecordError::Storage(error));
        }
        self.units = self.units.saturating_add(1);
        self.bytes = used;
        source.last_sequence = Some(last);
        Ok(Recorded::Stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn create(case: &str) -> Self {
            for attempt in 0..16 {
                let path = std::env::temp_dir().join(format!(
                    "quic-recorder-{}-{case}-{attempt}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("recorder fixture directory: {error}"),
                }
            }
            panic!("no free recorder fixture directory");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn opus(sequence: u16) -> ReceivedOpus {
        ReceivedOpus {
            publication_id: "pub_audio".into(),
            sequence,
            timestamp: 960,
            marker: false,
            payload: vec![0xf8, 0xff, 0xfe],
        }
    }

    #[test]
    fn records_bytes_and_rtp_coordinates_until_the_unit_limit() {
        let scratch = Scratch::create("units");
        let directory = scratch.0.join("capture");
        let mut capture = Recorder::create(
            &directory,
            Some("pub_audio".into()),
            Some("pub_video".into()),
            Limits {
                units: 4,
                bytes: 4_096,
            },
        )
        .unwrap();
        assert_eq!(
            capture.record_opus(&opus(u16::MAX)).unwrap(),
            Recorded::Stored
        );
        assert_eq!(capture.record_opus(&opus(0)).unwrap(), Recorded::Stored);
        assert_eq!(capture.record_opus(&opus(2)).unwrap(), Recorded::Stored);
        let video = ReceivedH264 {
            publication_id: "pub_video".into(),
            timestamp: 9000,
            first_sequence: 4,
            last_sequence: 9,
            access_unit: vec![0, 0, 0, 1, 0x65],
        };
        assert_eq!(capture.record_h264(&video).unwrap(), Recorded::Stored);
        assert!(matches!(
            capture.record_h264(&video),
            Err(RecordError::Limit)
        ));
        assert!(matches!(
            capture.record_opus(&opus(3)),
            Err(RecordError::Stopped)
        ));
        assert_eq!(
            fs::read(directory.join("0000000003.h264")).unwrap(),
            video.access_unit
        );
        let index = fs::read_to_string(directory.join(INDEX)).unwrap();
        assert!(index.contains("pub_audio\t48000\t960\t0\t0\t0\t3"));
        assert!(index.contains("pub_audio\t48000\t960\t2\t2\t1\t3"));
        assert!(index.contains("pub_video\t90000\t9000\t4\t9\t0\t5"));
    }

    #[test]
    fn byte_limit_precedes_writes_and_storage_failure_fences_only_the_sink() {
        let scratch = Scratch::create("bytes");
        let directory = scratch.0.join("capture");
        let mut limited = Recorder::create(
            &directory,
            Some("pub_audio".into()),
            None,
            Limits {
                units: 2,
                bytes: HEADER.len() as u64,
            },
        )
        .unwrap();
        assert!(matches!(
            limited.record_opus(&opus(1)),
            Err(RecordError::Limit)
        ));
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        let directory = scratch.0.join("blocked");
        let mut blocked = Recorder::create(
            &directory,
            Some("pub_audio".into()),
            None,
            Limits {
                units: 2,
                bytes: 4_096,
            },
        )
        .unwrap();
        fs::create_dir(directory.join("0000000000.opus")).unwrap();
        assert!(matches!(
            blocked.record_opus(&opus(1)),
            Err(RecordError::Storage(_))
        ));
        assert!(matches!(
            blocked.record_opus(&opus(1)),
            Err(RecordError::Stopped)
        ));
        assert_eq!(fs::read_to_string(directory.join(INDEX)).unwrap(), HEADER);
    }
}
