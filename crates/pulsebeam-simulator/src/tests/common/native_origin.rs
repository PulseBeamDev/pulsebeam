//! Test-only source proof joining two EXISTING native observations. Never
//! infer decoded provenance from current catalog assignments or SSRC guesses.
use anyhow::{Context, Result, ensure};
use pulsebeam_agent_native::{OutputFrame, RemoteFrame, RemoteTrack};
use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

const PICTURES: usize = 512;
type Key = (u64, u64, u32); // receiving generation, native receiver, actual RTP time

#[derive(Clone, Debug, Default)]
enum Origin {
    #[default]
    Unseen,
    Unique(String),
    Unknown,
    Conflict,
}

#[derive(Clone, Debug)]
struct NativePicture {
    track: RemoteTrack,
    dimensions: (u32, u32),
    at: Instant,
    nontrivial: bool,
}

#[derive(Clone, Debug, Default)]
struct Picture {
    track: Option<RemoteTrack>,
    origin: Origin,
    native: Option<NativePicture>,
    credited: bool,
}

#[derive(Clone, Debug)]
struct Receipt {
    key: Key,
    origin: String,
    native: NativePicture,
}

#[derive(Debug)]
pub struct NativeOriginProof {
    pub track: RemoteTrack,
    pub dimensions: (u32, u32),
    pub frames: u64,
}

#[derive(Clone, Debug, Default)]
pub struct NativeOriginCorrelation {
    pictures: BTreeMap<Key, Picture>,
    order: VecDeque<Key>,
    receipts: VecDeque<Receipt>,
    latest: BTreeMap<(u64, u64), Key>,
}

impl NativeOriginCorrelation {
    fn key(frame: &RemoteFrame) -> Option<Key> {
        Some((
            frame.track.generation,
            frame.track.receiver,
            frame.metadata.rtp_timestamp?,
        ))
    }

    fn admit(&mut self, key: Key) {
        if self.pictures.contains_key(&key) {
            return;
        }
        if self.pictures.len() == PICTURES {
            let old = self.order.pop_front().expect("bounded picture order");
            self.pictures.remove(&old);
            self.receipts.retain(|receipt| receipt.key != old);
            self.latest.retain(|_, latest| *latest != old);
        }
        self.order.push_back(key);
        self.pictures.insert(key, Picture::default());
    }

    fn bind_track(&mut self, key: Key, track: &RemoteTrack) -> bool {
        self.admit(key);
        let picture = self.pictures.get_mut(&key).unwrap();
        if let Some(previous) = &picture.track {
            if previous != track {
                picture.origin = Origin::Conflict;
                self.receipts.retain(|receipt| receipt.key != key);
                return false;
            }
        } else {
            picture.track = Some(track.clone());
        }
        true
    }

    /// A missing encoded observation can conceal conflicting provenance. No
    /// pre-loss mapping or receipt remains admissible; only newly paired actual
    /// observations can establish a subsequent positive source witness.
    pub fn encoded_observation_lost(&mut self) {
        self.pictures.clear();
        self.order.clear();
        self.receipts.clear();
        self.latest.clear();
    }

    pub fn encoded(&mut self, frame: &RemoteFrame, origin: Option<&str>) {
        assert!(matches!(frame.frame, OutputFrame::H264 { .. }));
        let Some(key) = Self::key(frame) else {
            return;
        };
        if !self.bind_track(key, &frame.track) {
            return;
        }
        let picture = self.pictures.get_mut(&key).unwrap();
        picture.origin = match (&picture.origin, origin) {
            (Origin::Unseen, Some(origin)) => Origin::Unique(origin.to_owned()),
            (Origin::Unseen | Origin::Unknown, None) => Origin::Unknown,
            (Origin::Unique(prior), Some(origin)) if prior == origin => {
                Origin::Unique(prior.clone())
            }
            _ => Origin::Conflict,
        };
        // Later conflicting/unknown evidence revokes a previously paired
        // observation. These are bounded proof records, not native counters.
        if !matches!(picture.origin, Origin::Unique(_)) {
            self.receipts.retain(|receipt| receipt.key != key);
        }
        self.credit(key);
    }

    pub fn decoded(&mut self, frame: &RemoteFrame, at: Instant, nontrivial: bool) {
        let OutputFrame::I420 { frame: pixels } = &frame.frame else {
            panic!("native source proof requires actual I420 output");
        };
        let Some(key) = Self::key(frame) else {
            return;
        };
        self.latest.insert((key.0, key.1), key);
        if !self.bind_track(key, &frame.track) {
            return;
        }
        // Duplicate callbacks never become fresh output a second time.
        self.pictures
            .get_mut(&key)
            .unwrap()
            .native
            .get_or_insert_with(|| NativePicture {
                track: frame.track.clone(),
                dimensions: (pixels.width, pixels.height),
                at,
                nontrivial,
            });
        self.credit(key);
    }

    fn credit(&mut self, key: Key) {
        let picture = self.pictures.get_mut(&key).unwrap();
        if !picture.credited
            && let Origin::Unique(origin) = &picture.origin
            && let Some(native) = &picture.native
            && native.nontrivial
        {
            picture.credited = true;
            self.receipts.push_back(Receipt {
                key,
                origin: origin.clone(),
                native: native.clone(),
            });
        }
    }

    /// Source cadence is deliberately not invented here. This proves genuine
    /// new I420 observations during the stated interval, including still/1fps
    /// sources. Continuous camera cadence remains a separate native oracle.
    pub fn require_since(
        &self,
        origin: &str,
        generation: u64,
        since: Instant,
        min_frames: u64,
        min_dimensions: (u32, u32),
    ) -> Result<NativeOriginProof> {
        ensure!(
            min_frames > 0,
            "native origin proof requires positive progress"
        );
        for (receiver, key) in &self.latest {
            if receiver.0 != generation {
                continue;
            }
            let picture = self
                .pictures
                .get(key)
                .context("latest native picture was evicted")?;
            let Origin::Unique(current) = &picture.origin else {
                continue;
            };
            if current != origin {
                continue;
            }
            let native = picture
                .native
                .as_ref()
                .context("native picture unavailable")?;
            if !native.nontrivial
                || native.at <= since
                || native.dimensions.0 < min_dimensions.0
                || native.dimensions.1 < min_dimensions.1
            {
                continue;
            }
            let count = self
                .receipts
                .iter()
                .filter(|receipt| {
                    receipt.origin == origin
                        && (receipt.key.0, receipt.key.1) == *receiver
                        && receipt.native.at > since
                        && receipt.native.dimensions.0 >= min_dimensions.0
                        && receipt.native.dimensions.1 >= min_dimensions.1
                })
                .count();
            if u64::try_from(count)? >= min_frames {
                return Ok(NativeOriginProof {
                    track: native.track.clone(),
                    dimensions: native.dimensions,
                    frames: u64::try_from(count)?,
                });
            }
        }
        // Retain only bounded observation metadata in a failed witness. In
        // particular, distinguish missing native output from a pending or
        // conflicting encoded-source correlation without dumping pixel data.
        let latest: Vec<_> = self
            .latest
            .iter()
            .take(8)
            .map(|(receiver, key)| (receiver, key, self.pictures.get(key)))
            .collect();
        let matching = self
            .receipts
            .iter()
            .filter(|receipt| receipt.origin == origin && receipt.key.0 == generation)
            .count();
        anyhow::bail!(
            "no fresh native I420 source proof for {origin}, generation {generation}, minimum {min_frames} frames at {min_dimensions:?}; pictures={}, matching_receipts={matching}, latest={latest:?}",
            self.pictures.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_agent_native::{FrameMetadata, I420Frame};
    use std::time::Duration;

    // Synthetic owned OBSERVATION records for oracle negatives only. Never
    // injected into RTC; actual native encoded+I420 delivery has separate gates.
    fn frame(receiver: u64, timestamp: u32, decoded: bool) -> RemoteFrame {
        RemoteFrame {
            track: RemoteTrack {
                generation: 1,
                receiver,
                track_id: "unit".into(),
                mid: "1".into(),
                media_index: 1,
            },
            metadata: FrameMetadata {
                rtp_timestamp: Some(timestamp),
                ..FrameMetadata::default()
            },
            frame: if decoded {
                OutputFrame::I420 {
                    frame: I420Frame {
                        width: 4,
                        height: 4,
                        timestamp_us: 0,
                        rotation_degrees: 0,
                        data: vec![0; 24],
                    },
                }
            } else {
                OutputFrame::H264 {
                    timestamp,
                    keyframe: true,
                    payload: vec![],
                }
            },
        }
    }

    #[test]
    fn both_callback_orders_preserve_real_output_time_and_do_not_count_duplicates() {
        let start = Instant::now();
        let output = start + Duration::from_millis(10);
        for decoded_first in [false, true] {
            let mut log = NativeOriginCorrelation::default();
            if decoded_first {
                log.decoded(&frame(10, 1, true), output, true);
            }
            log.encoded(&frame(10, 1, false), Some("admission/v0"));
            if !decoded_first {
                log.decoded(&frame(10, 1, true), output, true);
            }
            assert!(
                log.require_since("admission/v0", 1, start, 1, (4, 4))
                    .is_ok()
            );
            let after_output = output + Duration::from_millis(1);
            assert!(
                log.require_since("admission/v0", 1, after_output, 1, (4, 4))
                    .is_err()
            );
            log.decoded(&frame(10, 1, true), after_output, true);
            assert!(
                log.require_since("admission/v0", 1, after_output, 1, (4, 4))
                    .is_err()
            );
            assert!(
                log.require_since("admission/v0", 1, start, 2, (4, 4))
                    .is_err()
            );
            assert!(
                log.require_since("admission/v0", 1, start, 1, (8, 8))
                    .is_err()
            );
        }
    }

    #[test]
    fn missing_unknown_conflicting_or_cross_receiver_evidence_cannot_claim_an_origin() {
        let start = Instant::now();
        let output = start + Duration::from_millis(1);
        let mut log = NativeOriginCorrelation::default();
        log.encoded(&frame(10, 1, false), Some("old/v0"));
        log.decoded(&frame(11, 1, true), output, true);
        assert!(log.require_since("old/v0", 1, start, 1, (4, 4)).is_err());
        let mut other_generation = frame(10, 1, true);
        other_generation.track.generation = 2;
        log.decoded(&other_generation, output, true);
        assert!(log.require_since("old/v0", 2, start, 1, (4, 4)).is_err());
        log.decoded(&frame(10, 1, true), output, true);
        assert!(log.require_since("old/v0", 1, start, 1, (4, 4)).is_ok());
        assert!(log.require_since("new/v0", 1, start, 1, (4, 4)).is_err());
        log.encoded(&frame(10, 1, false), Some("new/v0"));
        assert!(log.require_since("old/v0", 1, start, 1, (4, 4)).is_err());
        assert!(log.require_since("new/v0", 1, start, 1, (4, 4)).is_err());
        log.encoded(&frame(10, 2, false), Some("new/v0"));
        log.decoded(&frame(10, 2, true), output, true);
        assert!(log.require_since("new/v0", 1, start, 1, (4, 4)).is_ok());
        log.encoded(&frame(10, 3, false), None);
        log.decoded(&frame(10, 3, true), output, true);
        assert!(
            log.require_since("new/v0", 1, start, 1, (4, 4)).is_err(),
            "the latest raw output is not attributed"
        );
        let mut missing = frame(10, 4, false);
        missing.metadata.rtp_timestamp = None;
        log.encoded(&missing, Some("new/v0"));
        log.decoded(&frame(10, 4, true), output, true);
        assert!(log.require_since("new/v0", 1, start, 1, (4, 4)).is_err());
    }

    #[test]
    fn track_identity_must_match_even_if_the_native_rtp_key_collides() {
        let start = Instant::now();
        let output = start + Duration::from_millis(1);
        for field in 0..3 {
            for decoded_first in [false, true] {
                let encoded = frame(10, 1, false);
                let mut decoded = frame(10, 1, true);
                match field {
                    0 => decoded.track.track_id = "another-native-track".into(),
                    1 => decoded.track.mid = "another-mid".into(),
                    _ => decoded.track.media_index += 1,
                }
                let mut log = NativeOriginCorrelation::default();
                if decoded_first {
                    log.decoded(&decoded, output, true);
                }
                log.encoded(&encoded, Some("admission/v0"));
                if !decoded_first {
                    log.decoded(&decoded, output, true);
                }
                assert!(
                    log.require_since("admission/v0", 1, start, 1, (4, 4))
                        .is_err(),
                    "different native track identities cannot be paired"
                );

                let mut log = NativeOriginCorrelation::default();
                log.encoded(&encoded, Some("admission/v0"));
                log.decoded(&frame(10, 1, true), output, true);
                assert!(
                    log.require_since("admission/v0", 1, start, 1, (4, 4))
                        .is_ok()
                );
                let mut conflicting = encoded.clone();
                conflicting.track = decoded.track;
                log.encoded(&conflicting, Some("admission/v0"));
                assert!(
                    log.require_since("admission/v0", 1, start, 1, (4, 4))
                        .is_err(),
                    "conflicting track evidence must revoke a prior receipt"
                );
            }
        }
    }

    #[test]
    fn nontrivial_output_and_bounded_retained_evidence_are_mandatory() {
        let start = Instant::now();
        let output = start + Duration::from_millis(1);
        let mut log = NativeOriginCorrelation::default();
        log.encoded(&frame(10, 1, false), Some("admission/v0"));
        log.decoded(&frame(10, 1, true), output, false);
        assert!(
            log.require_since("admission/v0", 1, start, 1, (4, 4))
                .is_err()
        );
        log.encoded(&frame(10, 2, false), Some("admission/v0"));
        log.decoded(&frame(10, 2, true), output, true);
        assert!(
            log.require_since("admission/v0", 1, start, 1, (4, 4))
                .is_ok()
        );
        log.encoded_observation_lost();
        assert!(
            log.require_since("admission/v0", 1, start, 1, (4, 4))
                .is_err()
        );
        log.encoded(&frame(10, 2, false), Some("admission/v0"));
        assert!(
            log.require_since("admission/v0", 1, start, 1, (4, 4))
                .is_err(),
            "pre-loss native output must not be reused"
        );
        log.decoded(&frame(10, 2, true), output, true);
        for timestamp in 3..=u32::try_from(PICTURES + 2).unwrap() {
            log.encoded(&frame(10, timestamp, false), Some("admission/v0"));
        }
        assert_eq!(log.pictures.len(), PICTURES);
        assert!(
            log.require_since("admission/v0", 1, start, 1, (4, 4))
                .is_err(),
            "eviction cannot manufacture a witness"
        );
    }
}
