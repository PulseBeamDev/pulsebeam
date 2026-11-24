//! Bounded reference storage for one pinned encoded subscription.
//!
//! Keep these synchronous filesystem operations outside Agent turns. The sink
//! does not mux, synchronize clocks, or promise crash durability. Stop on a
//! storage failure; a partial tail can remain.

use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
};

use super::{rtp::Binding, timed::Receive};
use crate::media::units::{Codec, Event, SenderTime};

const INDEX: &str = "index.tsv";
const HEADER: &str = "event\tepoch\tpublication_id\tfile\tclock_hz\trtp_time\tfirst_seq\tlast_seq\tsender_rtp\tsender_ntp\tdetail\tbytes\n";

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub units: u32,
    /// Total logical bytes across the payloads and index.
    pub bytes: u64,
}

#[derive(Debug)]
pub enum RecordError {
    InvalidConfiguration,
    InvalidEvent,
    Limit,
    Storage(io::Error),
    Stopped,
}

pub struct Recorder {
    directory: PathBuf,
    index: File,
    publication: String,
    codec: Codec,
    limits: Limits,
    units: u32,
    bytes: u64,
    binding: Option<Binding>,
    epoch: u64,
    stopped: bool,
}

impl Recorder {
    /// Create a new directory for a pinned publication. The caller must keep
    /// events in reader order, including across binding changes.
    pub fn create(
        directory: &Path,
        publication: String,
        codec: Codec,
        limits: Limits,
    ) -> Result<Self, RecordError> {
        let bytes = HEADER.len() as u64;
        if limits.units == 0
            || limits.bytes < bytes
            || publication.is_empty()
            || publication.len() > 64
            || !publication
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(RecordError::InvalidConfiguration);
        }
        fs::create_dir(directory).map_err(RecordError::Storage)?;
        let mut index = File::create_new(directory.join(INDEX)).map_err(RecordError::Storage)?;
        index
            .write_all(HEADER.as_bytes())
            .map_err(RecordError::Storage)?;
        Ok(Self {
            directory: directory.into(),
            index,
            publication,
            codec,
            limits,
            units: 0,
            bytes,
            binding: None,
            epoch: 0,
            stopped: false,
        })
    }

    pub fn record(&mut self, binding: Binding, received: &Receive) -> Result<(), RecordError> {
        if self.stopped {
            return Err(RecordError::Stopped);
        }
        let changed = self.binding != Some(binding);
        let epoch = self.epoch.saturating_add(u64::from(changed));
        let id = &self.publication;
        let hz = self.codec.clock_rate();
        let (row, file) = match received {
            Receive::Media(Event::Unit(unit)) => {
                let (extension, limit) = match self.codec {
                    Codec::Opus => ("opus", pulsebeam_core::fabric::MAX_WAN_RTP_BYTES),
                    Codec::H264 => ("h264", pulsebeam_core::h264::MAX_ACCESS_UNIT_SIZE),
                };
                let payload = unit.data.as_slice();
                if unit.codec != self.codec
                    || unit.source != binding
                    || payload.is_empty()
                    || payload.len() > limit
                    || unit.first_sequence > unit.last_sequence
                {
                    return Err(RecordError::InvalidEvent);
                }
                let file = format!("{:010}.{extension}", self.units);
                let timestamp = coordinate(unit.rtp_time);
                let (sender_rtp, sender_ntp) = match unit.sender_time {
                    SenderTime::Unknown => ("-".into(), "-".into()),
                    SenderTime::Report { rtp_anchor, ntp } => {
                        (rtp_anchor.to_string(), format!("{ntp:016x}"))
                    }
                };
                let (first, last, access, size) = (
                    unit.first_sequence,
                    unit.last_sequence,
                    unit.access,
                    payload.len(),
                );
                let line = format!(
                    "unit\t{epoch}\t{id}\t{file}\t{hz}\t{timestamp}\t{first}\t{last}\t{sender_rtp}\t{sender_ntp}\t{access:?}\t{size}\n"
                );
                (line, Some((file, payload)))
            }
            Receive::Media(Event::Discontinuity(gap))
                if gap.source == binding && !gap.sequence.is_empty() =>
            {
                let first = gap.sequence.start;
                let last = gap.sequence.end.saturating_sub(1);
                let cause = gap.cause;
                (
                    format!(
                        "gap\t{epoch}\t{id}\t-\t{hz}\t-\t{first}\t{last}\t-\t-\t{cause:?}\t0\n"
                    ),
                    None,
                )
            }
            Receive::Loss(loss) if loss.first_sequence <= loss.last_sequence => {
                let (first, last) = (loss.first_sequence, loss.last_sequence);
                let (overflow, expired, invalidated) =
                    (loss.overflow, loss.expired, loss.invalidated);
                (
                    format!(
                        "loss\t{epoch}\t{id}\t-\t{hz}\t-\t{first}\t{last}\t-\t-\toverflow={overflow};expired={expired};invalidated={invalidated}\t0\n"
                    ),
                    None,
                )
            }
            Receive::Media(Event::Discontinuity(_)) | Receive::Loss(_) => {
                return Err(RecordError::InvalidEvent);
            }
        };
        let reset = if changed {
            format!("reset\t{epoch}\t{id}\t-\t{hz}\t-\t-\t-\t-\t-\tbinding\t0\n")
        } else {
            String::new()
        };
        let used = self
            .bytes
            .saturating_add(reset.len() as u64)
            .saturating_add(row.len() as u64)
            .saturating_add(file.as_ref().map_or(0, |(_, payload)| payload.len() as u64));
        if used > self.limits.bytes || (file.is_some() && self.units >= self.limits.units) {
            self.stopped = true;
            return Err(RecordError::Limit);
        }
        let write = (|| {
            if let Some((name, payload)) = &file {
                File::create_new(self.directory.join(name))?.write_all(payload)?;
            }
            self.index.write_all(reset.as_bytes())?;
            self.index.write_all(row.as_bytes())
        })();
        if let Err(error) = write {
            self.stopped = true;
            return Err(RecordError::Storage(error));
        }
        self.units = self.units.saturating_add(u32::from(file.is_some()));
        self.binding = Some(binding);
        self.epoch = epoch;
        self.bytes = used;
        Ok(())
    }
}

fn coordinate(value: Option<u64>) -> String {
    value.map_or_else(|| "-".into(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{
        jitter::GapReason,
        units::{Access, Cause, Dependency, Discontinuity, Unit},
    };
    use std::num::NonZeroU16;

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

    #[allow(
        clippy::disallowed_methods,
        reason = "the sink does not read the observation clock"
    )]
    fn unit(source: Binding, sequence: u64, time: Option<u64>) -> Receive {
        Receive::Media(Event::Unit(Unit {
            source,
            codec: Codec::Opus,
            first_sequence: sequence,
            last_sequence: sequence,
            rtp_time: time,
            sender_time: SenderTime::Unknown,
            arrived_at: std::time::Instant::now(),
            access: Access::Audio,
            dependency: Dependency::Absent,
            data: vec![0xf8, 0xff, 0xfe],
        }))
    }

    fn binding(id: u16) -> Binding {
        Binding {
            generation: crate::executor::tests::dial_effect().generation,
            id: NonZeroU16::new(id).unwrap(),
        }
    }

    fn create(directory: &Path, bytes: u64) -> Recorder {
        Recorder::create(
            directory,
            "pub_audio".into(),
            Codec::Opus,
            Limits { units: 2, bytes },
        )
        .unwrap()
    }

    #[test]
    fn indexes_complete_units_gaps_and_binding_resets() {
        let scratch = Scratch::create("units");
        let directory = scratch.0.join("capture");
        let mut recorder = create(&directory, 4_096);
        let first = binding(1);
        let next = binding(2);
        recorder
            .record(first, &unit(first, 65535, Some(4_294_967_300)))
            .unwrap();
        recorder
            .record(
                first,
                &Receive::Media(Event::Discontinuity(Discontinuity {
                    source: first,
                    sequence: 65536..65539,
                    cause: Cause::Gap(GapReason::Deadline),
                })),
            )
            .unwrap();
        recorder.record(next, &unit(next, 0, None)).unwrap();
        assert!(matches!(
            recorder.record(next, &unit(next, 1, Some(960))),
            Err(RecordError::Limit)
        ));
        assert_eq!(
            fs::read(directory.join("0000000001.opus")).unwrap(),
            [0xf8, 0xff, 0xfe]
        );
        let index = fs::read_to_string(directory.join(INDEX)).unwrap();
        assert!(index.contains(
            "unit\t1\tpub_audio\t0000000000.opus\t48000\t4294967300\t65535\t65535\t-\t-\tAudio\t3"
        ));
        assert!(
            index.contains("gap\t1\tpub_audio\t-\t48000\t-\t65536\t65538\t-\t-\tGap(Deadline)\t0")
        );
        assert!(index.contains("reset\t2\tpub_audio"));
        assert!(index.contains("unit\t2\tpub_audio\t0000000001.opus\t48000\t-\t0\t0"));
    }

    #[test]
    fn byte_limit_precedes_writes_and_storage_failure_fences_only_the_sink() {
        let scratch = Scratch::create("bytes");
        let directory = scratch.0.join("capture");
        let mut limited = create(&directory, HEADER.len() as u64);
        let live = binding(1);
        assert!(matches!(
            limited.record(live, &unit(live, 1, Some(960))),
            Err(RecordError::Limit)
        ));
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        let directory = scratch.0.join("blocked");
        let mut blocked = create(&directory, 4_096);
        fs::create_dir(directory.join("0000000000.opus")).unwrap();
        assert!(matches!(
            blocked.record(live, &unit(live, 1, Some(960))),
            Err(RecordError::Storage(_))
        ));
        assert!(matches!(
            blocked.record(live, &unit(live, 1, Some(960))),
            Err(RecordError::Stopped)
        ));
        assert_eq!(fs::read_to_string(directory.join(INDEX)).unwrap(), HEADER);
    }
}
