//! Adapter from actual decoded corpus observations to offered-timeline-v1.
use super::*;

#[derive(Clone, Copy)]
pub(super) enum CodecClock {
    Direct90k,
    NativeMilliseconds,
}
struct Decoded {
    ready: presentation::Ready,
    view: Vec<u8>,
    timestamp: u32,
}
pub(super) struct Observations {
    offered: Vec<presentation::Capture>,
    decoded: Vec<Decoded>,
    seen: BTreeSet<(presentation::CaptureId, usize)>,
    anchors: [Option<(u32, u32)>; 3],
    pub(super) invalid_identity: u64,
    pub(super) invalid_timestamp: u64,
    pub(super) duplicates: u64,
}
pub(super) struct Report {
    decoded_error: Vec<u64>,
    tick_error: Vec<Option<u64>>,
    pub(super) timeline: presentation::Report,
    pub(super) decoded_representation_squared_error: u64,
    pub(super) decoded_representation_samples: u64,
    pub(super) presented_squared_error_times_ns: u128,
    pub(super) presented_samples_times_ns: u128,
}
impl CodecClock {
    fn ticks(self, capture: u64, offered_ns: u64) -> u32 {
        match self {
            Self::Direct90k => u32::try_from(capture).unwrap() * corpus::RTP_STEP,
            Self::NativeMilliseconds => u32::try_from(offered_ns / 1_000_000).unwrap() * 90,
        }
    }
}
impl Observations {
    pub(super) fn coverage(&self, cutoff_ns: u64) -> Vec<presentation::DecodeStatus> {
        let mut first: Vec<Option<u64>> = vec![None; self.offered.len()];
        for output in &self.decoded {
            if output.ready.at_ns <= cutoff_ns {
                let time = &mut first[usize::try_from(output.ready.id.capture).unwrap()];
                *time = Some(time.map_or(output.ready.at_ns, |old| old.min(output.ready.at_ns)));
            }
        }
        self.offered
            .iter()
            .zip(first)
            .map(|(capture, ready)| {
                presentation::decode_status(capture.at_ns, ready, 180_000_000, false)
            })
            .collect()
    }
    pub(super) fn trace(&self, report: Option<&Report>) -> serde_json::Value {
        use serde_json::json;
        json!({"offered": self.offered.iter().map(|capture| json!({"id": [capture.id.source, capture.id.lifetime, capture.id.epoch, capture.id.capture], "capture_ns": capture.at_ns})).collect::<Vec<_>>(),
            "decoded": self.decoded.iter().enumerate().map(|(index, output)| json!({"id": [output.ready.id.source, output.ready.id.lifetime, output.ready.id.epoch, output.ready.id.capture],
                "layer": format!("{:?}", output.ready.layer), "ready_ns": output.ready.at_ns, "sequence": output.ready.sequence, "rtp_timestamp": output.timestamp,
                "reference_squared_error": report.map(|report| report.decoded_error[index]), "reference_samples": output.view.len()})).collect::<Vec<_>>(),
            "presentation": report.map(|report| report.timeline.ticks.iter().enumerate().map(|(index, tick)| json!({
                "at_ns": tick.at_ns, "duration_ns": tick.duration_ns, "reference_capture": self.offered[tick.reference].id.capture,
                "ready_sequence": tick.ready.map(|ready| self.decoded[ready].ready.sequence), "held": tick.held,
                "age_ns": tick.age_ns, "reference_squared_error": report.tick_error[index]})).collect::<Vec<_>>())})
    }
    pub(super) fn new(corpus: &corpus::Corpus) -> Self {
        Self {
            offered: (0..corpus.len())
                .map(|index| corpus.offered(index))
                .collect(),
            decoded: Vec::new(),
            seen: BTreeSet::new(),
            anchors: [None; 3],
            invalid_identity: 0,
            invalid_timestamp: 0,
            duplicates: 0,
        }
    }

    pub(super) fn decoded(
        &mut self,
        frame: &VideoFrame,
        layer: usize,
        ready_ns: u64,
        clock: CodecClock,
    ) {
        if layer >= 3
            || (frame.width as usize, frame.height as usize) != corpus::LAYERS[layer].dimensions()
        {
            self.invalid_identity += 1;
            return;
        }
        let scope = self.offered[0].id;
        let Some(id) = corpus::identify(frame, scope.source, scope.lifetime) else {
            self.invalid_identity += 1;
            return;
        };
        let Some(offered) = usize::try_from(id.capture)
            .ok()
            .and_then(|index| self.offered.get(index))
        else {
            self.invalid_identity += 1;
            return;
        };
        if offered.id != id || offered.at_ns > ready_ns {
            self.invalid_identity += 1;
            return;
        }
        let ticks = clock.ticks(id.capture, offered.at_ns);
        let (base_ticks, base_timestamp) =
            *self.anchors[layer].get_or_insert((ticks, frame.rtp_timestamp));
        if frame.rtp_timestamp.wrapping_sub(base_timestamp) != ticks.wrapping_sub(base_ticks) {
            self.invalid_timestamp += 1;
            return;
        }
        if !self.seen.insert((id, layer)) {
            self.duplicates += 1;
            return;
        }
        let layer_name = [
            presentation::Layer::Q,
            presentation::Layer::H,
            presentation::Layer::F,
        ][layer];
        let ready = presentation::Ready {
            id,
            layer: layer_name,
            at_ns: ready_ns,
            sequence: u64::try_from(self.decoded.len()).unwrap(),
        };
        self.decoded.push(Decoded {
            ready,
            view: corpus::common_view(frame, layer),
            timestamp: frame.rtp_timestamp,
        });
    }

    pub(super) fn finish(
        &self,
        corpus: &corpus::Corpus,
        cutoff_ns: u64,
        evidence_complete: bool,
    ) -> Report {
        let ready: Vec<_> = self.decoded.iter().map(|output| output.ready).collect();
        let timeline = presentation::measure(
            &self.offered,
            &ready,
            presentation::Policy {
                horizon_ns: 6_000_000_000,
                cutoff_ns,
                // Sampling policy, not a claim about a browser or hardware display.
                tick_ns: 10_000_000,
                deadline_ns: 180_000_000,
                freeze_threshold_ns: 200_000_000,
                max_ticks: 600,
                evidence_complete: evidence_complete
                    && self.invalid_identity == 0
                    && self.invalid_timestamp == 0,
            },
        )
        .unwrap();
        let mut references = BTreeMap::new();
        let mut decoded_representation_squared_error = 0u64;
        let mut decoded_representation_samples = 0u64;
        let mut errors = BTreeMap::new();
        let mut decoded_error = Vec::new();
        let mut tick_error = Vec::new();
        for (index, output) in self.decoded.iter().enumerate() {
            let truth = reference(
                &mut references,
                corpus,
                usize::try_from(output.ready.id.capture).unwrap(),
            );
            let error = squared_error(&output.view, truth);
            errors.insert(
                (usize::try_from(output.ready.id.capture).unwrap(), index),
                error,
            );
            decoded_error.push(error);
            decoded_representation_squared_error += error;
            decoded_representation_samples += u64::try_from(truth.len()).unwrap();
        }
        let mut presented_squared_error_times_ns = 0u128;
        let mut presented_samples_times_ns = 0u128;
        for tick in &timeline.ticks {
            if let Some(index) = tick.ready {
                let truth = reference(&mut references, corpus, tick.reference);
                let error = *errors
                    .entry((tick.reference, index))
                    .or_insert_with(|| squared_error(&self.decoded[index].view, truth));
                tick_error.push(Some(error));
                presented_squared_error_times_ns +=
                    u128::from(error) * u128::from(tick.duration_ns);
                presented_samples_times_ns +=
                    u128::try_from(truth.len()).unwrap() * u128::from(tick.duration_ns);
            } else {
                tick_error.push(None);
            }
        }
        Report {
            decoded_error,
            tick_error,
            timeline,
            decoded_representation_squared_error,
            decoded_representation_samples,
            presented_squared_error_times_ns,
            presented_samples_times_ns,
        }
    }
}

fn reference<'a>(
    references: &'a mut BTreeMap<usize, Vec<u8>>,
    corpus: &corpus::Corpus,
    index: usize,
) -> &'a [u8] {
    references.entry(index).or_insert_with(|| {
        corpus
            .frame(index)
            .rung(pulsebeam_testdata::QualityVideoLayer::P180)
    })
}

fn squared_error(actual: &[u8], reference: &[u8]) -> u64 {
    assert_eq!(actual.len(), reference.len());
    actual
        .iter()
        .zip(reference)
        .map(|(a, b)| u64::from(a.abs_diff(*b)).pow(2))
        .sum()
}
