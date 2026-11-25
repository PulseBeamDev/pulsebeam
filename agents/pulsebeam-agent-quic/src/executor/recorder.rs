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

use pulsebeam_core::dd::DecodeTargetIndication;

use super::{rtp::Binding, timed::Receive};
use crate::media::units::{Codec, Dependency, Event, Framing, SenderTime, Unit};

const INDEX: &str = "index.tsv";
const HEADER: &str = "event\tepoch\tpublication_id\tfile\tclock_hz\trtp_time\tfirst_seq\tlast_seq\tsender_rtp\tsender_ntp\tdetail\tbytes\tdd_frame\tspatial_id\ttemporal_id\tactive_targets\tframe_targets\n";
const NO_VIDEO_COORDINATES: &str = "\t-\t-\t-\t-\t-\n";

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
    active_targets: Option<u32>,
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
            active_targets: None,
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
        let mut active_targets = if changed { None } else { self.active_targets };
        let (row, file) = match received {
            Receive::Media(Event::Unit(unit)) => {
                let (extension, limit) = match self.codec {
                    Codec::Opus => ("opus", pulsebeam_core::fabric::MAX_WAN_RTP_BYTES),
                    Codec::H264 => ("h264", pulsebeam_core::h264::MAX_ACCESS_UNIT_SIZE),
                    Codec::Vp8 => ("vp8", pulsebeam_core::h264::MAX_ACCESS_UNIT_SIZE),
                    Codec::Vp9 => ("vp9", pulsebeam_core::h264::MAX_ACCESS_UNIT_SIZE),
                    Codec::Av1 => ("obu", pulsebeam_core::h264::MAX_ACCESS_UNIT_SIZE),
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
                let video = video_coordinates(unit, &mut active_targets);
                let line = format!(
                    "unit\t{epoch}\t{id}\t{file}\t{hz}\t{timestamp}\t{first}\t{last}\t{sender_rtp}\t{sender_ntp}\t{access:?}\t{size}{video}"
                );
                (line, Some((file, payload)))
            }
            Receive::Media(Event::Discontinuity(gap))
                if gap.source == binding && !gap.sequence.is_empty() =>
            {
                let first = gap.sequence.start;
                let last = gap.sequence.end.saturating_sub(1);
                let cause = gap.cause;
                active_targets = None;
                (
                    format!(
                        "gap\t{epoch}\t{id}\t-\t{hz}\t-\t{first}\t{last}\t-\t-\t{cause:?}\t0{NO_VIDEO_COORDINATES}"
                    ),
                    None,
                )
            }
            Receive::Loss(loss) if loss.first_sequence <= loss.last_sequence => {
                let (first, last) = (loss.first_sequence, loss.last_sequence);
                let (overflow, expired, invalidated) =
                    (loss.overflow, loss.expired, loss.invalidated);
                active_targets = None;
                (
                    format!(
                        "loss\t{epoch}\t{id}\t-\t{hz}\t-\t{first}\t{last}\t-\t-\toverflow={overflow};expired={expired};invalidated={invalidated}\t0{NO_VIDEO_COORDINATES}"
                    ),
                    None,
                )
            }
            Receive::Media(Event::Discontinuity(_)) | Receive::Loss(_) => {
                return Err(RecordError::InvalidEvent);
            }
        };
        let reset = if changed {
            format!(
                "reset\t{epoch}\t{id}\t-\t{hz}\t-\t-\t-\t-\t-\tbinding\t0{NO_VIDEO_COORDINATES}"
            )
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
        self.active_targets = active_targets;
        self.epoch = epoch;
        self.bytes = used;
        Ok(())
    }
}

fn video_coordinates(unit: &Unit<Binding>, active: &mut Option<u32>) -> String {
    if unit.codec == Codec::Opus {
        return NO_VIDEO_COORDINATES.into();
    }
    let (spatial, temporal) = match &unit.framing {
        Framing::Vp9 { descriptor, .. } => descriptor.layer.map_or((None, None), |layer| {
            (Some(layer.spatial_id), Some(layer.temporal_id))
        }),
        Framing::Av1 { layer, .. } => (Some(layer.spatial_id), Some(layer.temporal_id)),
        Framing::Vp8 { descriptor, .. } => (Some(0), descriptor.temporal.map(|layer| layer.id)),
        Framing::Whole => (None, None),
    };
    let (frame, spatial, temporal, targets) = match &unit.dependency {
        Dependency::Parsed(dd) => {
            *active = dd
                .active_decode_targets
                .or_else(|| {
                    dd.attached_structure
                        .as_ref()
                        .map(|structure| structure.all_decode_targets_bitmask())
                })
                .or(*active);
            let present =
                dd.frame_dependencies
                    .dtis
                    .iter()
                    .enumerate()
                    .fold(0, |mask, (i, dti)| {
                        if *dti == DecodeTargetIndication::NotPresent {
                            mask
                        } else {
                            mask | (1_u32 << i)
                        }
                    });
            (
                Some(dd.frame_number),
                Some(dd.spatial_id()),
                Some(dd.temporal_id()),
                active.map(|mask| mask & present),
            )
        }
        other => {
            *active = None;
            let frame = match other {
                Dependency::MissingContext(mandatory) => Some(mandatory.frame_number),
                _ => None,
            };
            (frame, spatial, temporal, None)
        }
    };
    format!(
        "\t{}\t{}\t{}\t{}\t{}\n",
        coordinate(frame.map(u64::from)),
        coordinate(spatial.map(u64::from)),
        coordinate(temporal.map(u64::from)),
        coordinate(active.map(u64::from)),
        coordinate(targets.map(u64::from))
    )
}

fn coordinate(value: Option<u64>) -> String {
    value.map_or_else(|| "-".into(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{
        jitter::GapReason,
        units::{Access, Cause, Discontinuity},
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
            framing: crate::media::units::Framing::Whole,
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
    fn records_same_timestamp_spatial_units_and_does_not_carry_target_masks_over_gaps() {
        use pulsebeam_core::dd::{
            DecodeTargetIndication::{NotPresent, Required},
            DependencyDescriptor, FrameDependencyTemplate,
        };
        let scratch = Scratch::create("spatial");
        let directory = scratch.0.join("capture");
        let mut recorder = Recorder::create(
            &directory,
            "pub_video".into(),
            Codec::Vp9,
            Limits {
                units: 3,
                bytes: 4_096,
            },
        )
        .unwrap();
        let source = binding(1);
        let video = |sequence, number, spatial, active, lower| {
            let Receive::Media(Event::Unit(mut unit)) = unit(source, sequence, Some(3_000)) else {
                unreachable!();
            };
            unit.codec = Codec::Vp9;
            unit.access = Access::DependentVideo;
            unit.dependency = Dependency::Parsed(Box::new(DependencyDescriptor {
                frame_number: number,
                start_of_frame: true,
                end_of_frame: true,
                active_decode_targets: active,
                frame_dependencies: FrameDependencyTemplate {
                    spatial_id: spatial,
                    dtis: [lower, Required].into_iter().collect(),
                    ..Default::default()
                },
                ..Default::default()
            }));
            Receive::Media(Event::Unit(unit))
        };
        recorder
            .record(source, &video(100, 65_535, 0, Some(3), Required))
            .unwrap();
        recorder
            .record(source, &video(101, 0, 1, None, NotPresent))
            .unwrap();
        recorder
            .record(
                source,
                &Receive::Media(Event::Discontinuity(Discontinuity {
                    source,
                    sequence: 102..103,
                    cause: Cause::Gap(GapReason::Deadline),
                })),
            )
            .unwrap();
        recorder
            .record(source, &video(103, 2, 1, None, NotPresent))
            .unwrap();
        let index = fs::read_to_string(directory.join(INDEX)).unwrap();
        let rows = index
            .lines()
            .filter(|row| row.starts_with("unit\t"))
            .map(|row| row.split('\t').collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert!(rows.iter().all(|row| row[5] == "3000"));
        assert_eq!(&rows[0][12..], ["65535", "0", "0", "3", "3"]);
        assert_eq!(&rows[1][12..], ["0", "1", "0", "3", "2"]);
        assert_eq!(&rows[2][12..], ["2", "1", "0", "-", "-"]);
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
