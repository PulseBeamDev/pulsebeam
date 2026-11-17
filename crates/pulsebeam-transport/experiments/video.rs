//! Matched encoded-video comparison with native WebRTC/UDP as the reference.
use super::*;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const RTP_BASE: u32 = 90_000;
const FRAME_MS: u64 = 40;
const FRAME_RATE: u32 = 25;
const RTP_STEP: u32 = 3600;
const DRAIN_MS: u64 = 1500;

#[derive(Clone)]
struct EncodedPicture {
    bytes: Vec<u8>,
    key: bool,
    raw: Vec<u8>,
}
#[derive(Clone)]
struct Trace {
    pictures: Vec<EncodedPicture>,
    duration: u64,
    target_bps: u32,
}

pub(super) fn pixels(id: usize) -> Vec<u8> {
    let w = WIDTH as usize;
    let h = HEIGHT as usize;
    let mut data = vec![128; w * h * 3 / 2];
    for y in 0..h {
        for x in 0..w {
            let moving = ((x + id * 7) / 12) ^ ((y + id * 3) / 12);
            data[y * w + x] =
                u8::try_from(24 + (x * 5 + y * 3 + id * 11 + moving * 53) % 208).unwrap();
        }
    }
    // Decoded-picture identity, not optional ancillary metadata. Complementary
    // rows detect damaged/partially concealed identifiers.
    for bit in 0..16 {
        let one = id & (1 << bit) != 0;
        for row in 0..2 {
            for y in row * 16..(row + 1) * 16 {
                for x in bit * 16..(bit + 1) * 16 {
                    data[y * w + x] = if one ^ (row == 1) { 235 } else { 16 };
                }
            }
        }
    }
    data
}
pub(super) fn picture_id(frame: &VideoFrame) -> Option<usize> {
    if frame.width < 64 || frame.height < 36 {
        return None;
    }
    let bytes = frame.buffer.as_bytes();
    let mut id = 0;
    for bit in 0..16 {
        let sample = |row: usize| {
            let mut sum = 0u32;
            for y in row * 16 + 6..row * 16 + 10 {
                for x in bit * 16 + 6..bit * 16 + 10 {
                    sum += u32::from(
                        bytes[(y * frame.height as usize / HEIGHT as usize) * frame.width as usize
                            + x * frame.width as usize / WIDTH as usize],
                    );
                }
            }
            sum / 16
        };
        let a = sample(0);
        let b = sample(1);
        if a.abs_diff(b) < 100 {
            return None;
        }
        if a > b {
            id |= 1 << bit;
        }
    }
    Some(id)
}

use super::encoder::SourceEncoder;
use super::media_queue;
impl media_queue::PictureQueue {
    fn push(&mut self, trace: &Trace, id: usize, epoch: u64) {
        self.push_packets(id, epoch, fragments(trace, id, epoch));
    }
}
use media_queue::PictureQueue;

const VIDEO_HEADER: usize = 13;
const KEYFRAME_REQUEST: &[u8] = b"\xff\xff\xff\xffIDR";

const INITIAL_BPS: u32 = 300_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MediaSource {
    Fixed,
    Guided,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeSource {
    Fixed,
    Raw,
}
impl NativeSource {
    fn is_raw(self) -> bool {
        self != Self::Fixed
    }
}

impl Trace {
    fn encode(duration: u64, target_bps: u32) -> Self {
        let mut trace = Self::captures(duration, target_bps);
        let mut encoder = SourceEncoder::new(target_bps, target_bps, false);
        for id in 0..trace.pictures.len() {
            assert!(trace.encode_picture(
                &mut encoder,
                id,
                target_bps,
                f64::from(FRAME_RATE),
                id.is_multiple_of(60)
            ));
        }
        trace
    }
    fn captures(duration: u64, target_bps: u32) -> Self {
        Self {
            pictures: (0..usize::try_from(duration.div_ceil(FRAME_MS)).unwrap())
                .map(|id| EncodedPicture {
                    bytes: Vec::new(),
                    key: false,
                    raw: pixels(id),
                })
                .collect(),
            duration,
            target_bps,
        }
    }
    fn encode_picture(
        &mut self,
        encoder: &mut SourceEncoder,
        id: usize,
        target_bps: u32,
        fps: f64,
        key: bool,
    ) -> bool {
        let raw = VideoFrame::i420(
            WIDTH,
            HEIGHT,
            self.pictures[id].raw.clone(),
            10_000_000 + i64::try_from(id as u64 * FRAME_MS * 1000).unwrap(),
            RTP_BASE + u32::try_from(id).unwrap() * RTP_STEP,
        )
        .unwrap();
        let Some((bytes, key)) = encoder.encode(&raw, target_bps, fps, key) else {
            return false;
        };
        self.pictures[id].bytes = bytes;
        self.pictures[id].key = key;
        true
    }
    fn bytes(&self) -> u64 {
        self.pictures.iter().map(|p| p.bytes.len() as u64).sum()
    }
}

use super::decoder::Decoder;
impl Decoder {
    fn submit(&mut self, trace: &Trace, id: usize, missing: bool) {
        let picture = &trace.pictures[id];
        let mut image = EncodedImage::default();
        image.set_buffer(&EncodedVideoBuffer::from_bytes(&picture.bytes));
        let mut metadata = image.metadata();
        metadata.width = WIDTH;
        metadata.height = HEIGHT;
        metadata.rtp_timestamp = RTP_BASE + u32::try_from(id).unwrap() * RTP_STEP;
        metadata.frame_type = if picture.key {
            EncodedImageFrameType::Key
        } else {
            EncodedImageFrameType::Delta
        };
        image.set_metadata(&metadata).unwrap();
        self.decode_image(
            &image,
            missing,
            i64::try_from(id as u64 * FRAME_MS).unwrap(),
        );
        self.last_input = Some(id);
    }
}

#[derive(Default)]
struct VideoMetrics {
    decoded: BTreeMap<usize, u64>,
    invalid_identity: u64,
    duplicates: u64,
    complete_encoded: BTreeSet<usize>,
    parts: BTreeMap<usize, BTreeSet<u16>>,
    expired_fragments: u64,
    decoder_rejected: u64,
    reordered_encoded_discarded: u64,
    failed: Option<Error>,
    source_dropped: Option<u64>,
    native_input_pending: Option<u64>,
    native_encoded: Option<u64>,
    native_sent: Option<u64>,
    native_target_bps: Option<f64>,
    native_scream_feedback: Option<bool>,
    rtp_anchor: Option<(usize, u32)>,
    rtp_step: Option<u32>,
    timestamp_mismatches: u64,
    source_encoded: BTreeSet<usize>,
    source_targets: Vec<u32>,
    source_paused: u64,
    source_backpressured: u64,
    encoder_dropped: u64,
    source_keyframes: u64,
    keyframe_requests: u64,
    dependency_discarded: u64,
    admitted_pictures: u64,
    terminal_pictures: Vec<media_queue::TerminalPicture>,
}
impl VideoMetrics {
    fn decoded(&mut self, trace: &Trace, frame: VideoFrame, now: u64) {
        let Some(id) = picture_id(&frame).filter(|id| *id < trace.pictures.len()) else {
            self.invalid_identity += 1;
            return;
        };
        if let Some((anchor_id, anchor_timestamp)) = self.rtp_anchor {
            let Some(delta) = id
                .checked_sub(anchor_id)
                .and_then(|n| u32::try_from(n).ok())
            else {
                self.timestamp_mismatches += 1;
                return;
            };
            let ticks = frame.rtp_timestamp.wrapping_sub(anchor_timestamp);
            if self.rtp_step.is_none()
                && delta > 0
                && ticks.is_multiple_of(delta)
                && ticks / delta == RTP_STEP
            {
                // Both paths use exact 40 ms captures at the 90 kHz RTP clock.
                self.rtp_step = Some(ticks / delta);
            }
            if self
                .rtp_step
                .map_or(ticks != 0 || delta != 0, |step| ticks != delta * step)
            {
                self.timestamp_mismatches += 1;
                return;
            }
        } else {
            self.rtp_anchor = Some((id, frame.rtp_timestamp));
        }
        if self.decoded.contains_key(&id) {
            self.duplicates += 1;
            return;
        }
        assert!(
            now >= id as u64 * FRAME_MS,
            "decoded picture precedes capture"
        );
        self.decoded.insert(id, now);
    }
    fn receive(&mut self, trace: &Trace, bytes: &[u8], now: u64, decoder: &mut Decoder) {
        assert!(bytes.len() >= VIDEO_HEADER);
        let id = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        let previous = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
        let previous = (previous != u32::MAX).then_some(previous as usize);
        let part = u16::from_be_bytes(bytes[8..10].try_into().unwrap());
        let count = u16::from_be_bytes(bytes[10..12].try_into().unwrap()) as usize;
        let key = bytes[12] == 1;
        let source = &trace.pictures[id].bytes;
        assert_eq!(key, trace.pictures[id].key);
        assert_eq!(count, source.len().div_ceil(CHUNK));
        let offset = part as usize * CHUNK;
        assert_eq!(
            &bytes[VIDEO_HEADER..],
            &source[offset..(offset + CHUNK).min(source.len())]
        );
        let parts = self.parts.entry(id).or_default();
        if parts.insert(part) && parts.len() == count && self.complete_encoded.insert(id) {
            if decoder.last_input.is_some_and(|last| id <= last) {
                self.reordered_encoded_discarded += 1;
                return;
            }
            let missing = previous != decoder.last_input;
            if !key && (missing || decoder.waiting_for_key) {
                self.dependency_discarded += 1;
                decoder.waiting_for_key = true;
                return;
            }
            if key {
                decoder.waiting_for_key = false;
            }
            decoder.submit(trace, id, missing);
            while let Ok(frame) = decoder.output.try_recv() {
                self.decoded(trace, frame, now);
            }
        }
    }
    fn source_picture(&mut self, trace: &Trace, id: usize) {
        assert!(self.source_encoded.insert(id));
        self.source_keyframes += u64::from(trace.pictures[id].key);
    }
    fn assert_source_accounted(&self, trace: &Trace) {
        assert_eq!(
            self.source_encoded.len() as u64
                + self.source_paused
                + self.source_backpressured
                + self.encoder_dropped,
            trace.pictures.len() as u64,
            "every capture must be encoded or have a source-stage omission"
        );
    }

    fn max_decode_gap(&self, duration: u64) -> u64 {
        let mut arrivals: Vec<_> = self
            .decoded
            .values()
            .copied()
            .filter(|at| *at <= duration)
            .collect();
        arrivals.extend([0, duration]);
        arrivals.sort_unstable();
        arrivals
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .max()
            .unwrap()
    }
}

fn native_video(trace: &Trace, scenario: Scenario, seed: u64, adaptive: bool) -> VideoMetrics {
    native_video_mode(
        trace,
        scenario,
        seed,
        if adaptive {
            NativeSource::Raw
        } else {
            NativeSource::Fixed
        },
    )
}

fn native_video_mode(
    input: &Trace,
    scenario: Scenario,
    seed: u64,
    mode: NativeSource,
) -> VideoMetrics {
    native_video_controller(input, scenario, seed, mode, NativeController::ScreamV2)
}

fn native_video_controller(
    input: &Trace,
    scenario: Scenario,
    seed: u64,
    mode: NativeSource,
    controller: NativeController,
) -> VideoMetrics {
    let trace = input.clone();
    let start_bps = INITIAL_BPS.min(trace.target_bps);
    let mut native = Native::configured(
        seed,
        scenario,
        NativeOptions {
            decode_video: true,
            raw_video: mode.is_raw(),
            disable_frame_dropper: false,
            controller,
            initial_bps: start_bps,
            minimum_bps: Some(50_000),
        },
    );
    let mut parameters = native.sender.parameters().unwrap();
    assert_eq!(parameters.encodings.len(), 1);
    parameters.encodings[0].max_bitrate_bps = Some(trace.target_bps);
    parameters.encodings[0].max_framerate = Some(f64::from(FRAME_RATE));
    native.sender.set_parameters(parameters).unwrap();
    let epoch = u64::try_from(native.world.now().as_millis()).unwrap() - 10_000;
    native.link.begin_measurement(epoch, trace.duration, seed);
    let mut metrics = VideoMetrics::default();
    for elapsed in 0..trace.duration + DRAIN_MS {
        if elapsed < trace.duration && elapsed.is_multiple_of(FRAME_MS) {
            let id = (elapsed / FRAME_MS) as usize;
            if mode == NativeSource::Fixed {
                metrics.source_targets.push(trace.target_bps);
            }
            let picture = &trace.pictures[id];
            if let Some(source) = &native.raw_source {
                source
                    .push_frame(
                        &VideoFrame::i420(
                            WIDTH,
                            HEIGHT,
                            picture.raw.clone(),
                            native.world.now().as_micros().try_into().unwrap(),
                            RTP_BASE + u32::try_from(id).unwrap() * RTP_STEP,
                        )
                        .unwrap(),
                    )
                    .unwrap();
            } else {
                native
                    .source
                    .as_ref()
                    .unwrap()
                    .push_encoded(EncodedVideoAccessUnit {
                        data: picture.bytes.clone(),
                        width: WIDTH,
                        height: HEIGHT,
                        timestamp_us: native.world.now().as_micros().try_into().unwrap(),
                        key_frame: picture.key,
                        qp: None,
                        metadata: EncodedVideoMetadata {
                            codec: EncodedVideoCodec::H264 {
                                base_layer_sync: false,
                            },
                            simulcast_index: None,
                            spatial_index: None,
                            temporal_index: None,
                            end_of_picture: true,
                        },
                    })
                    .unwrap();
                metrics.source_picture(&trace, id);
            }
        }
        if scenario.turn(elapsed) {
            native.pump();
            native.events();
            while let Some((_, id)) = native.link.pop(elapsed) {
                native.network.deliver(id).unwrap();
            }
            native.pump();
            while let Some(packet) = native.network.next_packet() {
                assert_eq!(packet.kind, OutboundKind::Udp);
                let from = usize::from(packet.source.ip() != Ipv4Addr::new(10, 79, 0, 1));
                if let Err(id) =
                    native
                        .link
                        .push(elapsed, from, packet.payload.len() + 28, packet.id)
                {
                    native.network.drop_packet(id).unwrap();
                }
            }
            for sink in &native.decoded_sinks {
                assert!(!sink.observer_failed());
                while let Some(frame) = sink.try_next_frame() {
                    metrics.decoded(&trace, frame, elapsed);
                }
            }
            if let Some(source) = &native.source {
                assert!(source.take_encoder_error().is_none());
            }
        }
        native.world.advance(Duration::from_millis(1)).unwrap();
    }
    let request = native.peers[0]
        .request_sender_stats(&native.sender)
        .unwrap();
    let mut received_stats = false;
    for _ in 0..100 {
        native.pump();
        while let Some(event) = native.peers[0].try_next_event() {
            if let PeerConnectionEvent::Stats(snapshot) = event {
                assert_eq!(snapshot.operation_id, request);
                for record in snapshot.records {
                    if let PeerStatsRecord::OutboundRtp(stats) = record
                        && stats.frames_encoded.is_some()
                    {
                        assert!(metrics.native_encoded.is_none(), "unexpected simulcast");
                        metrics.native_encoded = stats.frames_encoded;
                        metrics.native_sent = stats.frames_sent;
                        metrics.native_target_bps = stats.target_bitrate;
                    }
                }
                received_stats = true;
            }
        }
        if received_stats {
            break;
        }
        native.world.advance(Duration::from_millis(1)).unwrap();
    }
    assert!(received_stats && metrics.native_encoded.is_some());
    let scream_feedback = native.scream_feedback.try_recv().is_ok();
    assert_eq!(
        scream_feedback,
        controller == NativeController::ScreamV2,
        "native controller selection requires actual SCReAM feedback events, not only a configured trial"
    );
    metrics.native_scream_feedback = Some(scream_feedback);
    metrics.source_dropped = native
        .source
        .as_ref()
        .map(EncodedVideoSource::dropped_frames);
    metrics.native_input_pending = native
        .source
        .as_ref()
        .map(|source| u64::try_from(source.pending_frames()).unwrap());
    if !mode.is_raw() {
        metrics.assert_source_accounted(&trace);
    }
    metrics
}

fn fragments(trace: &Trace, id: usize, epoch: u64) -> Vec<Packet> {
    let picture = &trace.pictures[id];
    let previous = trace.pictures[..id]
        .iter()
        .rposition(|picture| !picture.bytes.is_empty())
        .map_or(u32::MAX, |id| u32::try_from(id).unwrap());
    let count = picture.bytes.len().div_ceil(CHUNK);
    picture
        .bytes
        .chunks(CHUNK)
        .enumerate()
        .map(|(part, chunk)| {
            let mut bytes = Vec::with_capacity(chunk.len() + VIDEO_HEADER);
            bytes.extend_from_slice(&u32::try_from(id).unwrap().to_be_bytes());
            bytes.extend_from_slice(&previous.to_be_bytes());
            bytes.extend_from_slice(&u16::try_from(part).unwrap().to_be_bytes());
            bytes.extend_from_slice(&u16::try_from(count).unwrap().to_be_bytes());
            bytes.push(u8::from(picture.key));
            bytes.extend_from_slice(chunk);
            Packet {
                bytes,
                expires: Time((epoch + id as u64 * FRAME_MS + DEADLINE_MS) * 1_000_000),
            }
        })
        .collect()
}

fn quic_video(
    trace: &Trace,
    scenario: Scenario,
    seed: u64,
    cc: QuicCongestion,
    overlay: bool,
) -> VideoMetrics {
    quic_video_mode(trace, scenario, seed, cc, overlay, MediaSource::Fixed)
}

fn quic_video_mode(
    input: &Trace,
    scenario: Scenario,
    seed: u64,
    cc: QuicCongestion,
    overlay: bool,
    mode: MediaSource,
) -> VideoMetrics {
    datagram_video_mode(input, scenario, seed, MediaPath::Quic { cc, overlay }, mode)
}

use super::path::MediaPath;

fn datagram_video_mode(
    input: &Trace,
    scenario: Scenario,
    seed: u64,
    path: MediaPath,
    mode: MediaSource,
) -> VideoMetrics {
    let overlay = path.overlay();
    assert!(
        mode != MediaSource::Guided || overlay,
        "bare QUIC has no encoder-rate oracle"
    );
    let mut trace = if mode == MediaSource::Guided {
        Trace::captures(input.duration, input.target_bps)
    } else {
        input.clone()
    };
    let mut encoder = (mode == MediaSource::Guided)
        .then(|| SourceEncoder::new(INITIAL_BPS, trace.target_bps, true));
    let mut force_key = true;
    let mut last_key_request: Option<u64> = None;
    let addresses: [SocketAddr; 2] = [
        "127.0.0.1:41001".parse().unwrap(),
        "127.0.0.1:41002".parse().unwrap(),
    ];
    let origin = Instant::now();
    let mut peers = path.sessions(origin, addresses);
    let mut inbound = [VecDeque::<Vec<u8>>::new(), VecDeque::new()];
    let mut link = Link::new(scenario, seed, u64::MAX / 4);
    let mut epoch = 0;
    for at in 0..5000 {
        while let Some((to, bytes)) = link.pop(at) {
            inbound[to].push_back(bytes);
        }
        for who in 0..2 {
            for _ in 0..4096 {
                let mut output = Vec::new();
                peers[who].poll(at, &mut output, &mut Vec::new()).unwrap();
                for bytes in output {
                    let _ = link.push(at, who, bytes.len() + 28, bytes);
                }
                if !peers[who].input_ready() {
                    if peers[who].needs_progress() {
                        continue;
                    }
                    break;
                }
                let Some(bytes) = inbound[who].pop_front() else {
                    break;
                };
                peers[who].receive(addresses[1 - who], &bytes, at);
            }
        }
        if at >= 200
            && peers.iter().all(Session::ready)
            && inbound.iter().all(VecDeque::is_empty)
            && link.pending.is_empty()
        {
            epoch = at + 1;
            break;
        }
    }
    assert!(epoch > 0);
    link.begin_measurement(epoch, trace.duration, seed);
    let mut metrics = VideoMetrics::default();
    let mut decoder = Decoder::new();
    let mut queued = PictureQueue::default();
    for elapsed in 0..trace.duration + DRAIN_MS {
        let at = epoch + elapsed;
        while let Some((to, bytes)) = link.pop(elapsed) {
            inbound[to].push_back(bytes);
        }

        if elapsed < trace.duration && elapsed.is_multiple_of(FRAME_MS) {
            let id = (elapsed / FRAME_MS) as usize;
            let mut publish = true;
            if let Some(encoder) = encoder.as_mut() {
                let target = u32::try_from(
                    peers[0]
                        .encoder_target_bps()
                        .expect("guided source requires an application controller")
                        .min(u64::from(trace.target_bps)),
                )
                .unwrap();
                metrics.source_targets.push(target);
                force_key |= id.is_multiple_of(60);
                if target == 0 {
                    metrics.source_paused += 1;
                    publish = false;
                } else if !queued.is_empty() {
                    metrics.source_backpressured += 1;
                    publish = false;
                } else if !trace.encode_picture(
                    encoder,
                    id,
                    target,
                    f64::from(FRAME_RATE),
                    force_key,
                ) {
                    metrics.encoder_dropped += 1;
                    publish = false;
                } else {
                    force_key = false;
                }
            }
            if publish {
                if mode == MediaSource::Fixed {
                    metrics.source_targets.push(trace.target_bps);
                }
                metrics.source_picture(&trace, id);
                queued.push(&trace, id, epoch);
            }
        }
        if !scenario.turn(elapsed) {
            continue;
        }
        peers[0].demand(!queued.is_empty());
        for who in 0..2 {
            let mut settled = false;
            for _ in 0..4096 {
                let mut output = Vec::new();
                let mut received = Vec::new();
                if let Err(error) = peers[who].poll(at, &mut output, &mut received) {
                    metrics.failed = Some(error);
                    break;
                }
                for bytes in output {
                    let _ = link.push(elapsed, who, bytes.len() + 28, bytes);
                }
                for bytes in received {
                    if who == 1 {
                        metrics.receive(&trace, &bytes, elapsed, &mut decoder);
                    } else {
                        assert_eq!(bytes, KEYFRAME_REQUEST);
                        force_key = true;
                        metrics.keyframe_requests += 1;
                    }
                }
                if !peers[who].input_ready() {
                    if peers[who].needs_progress() {
                        continue;
                    }
                    settled = true;
                    break;
                }
                if let Some(bytes) = inbound[who].pop_front() {
                    peers[who].receive(addresses[1 - who], &bytes, at);
                    continue;
                }
                if who == 1
                    && mode == MediaSource::Guided
                    && decoder.waiting_for_key
                    && last_key_request.is_none_or(|last| elapsed.saturating_sub(last) >= 500)
                {
                    let request = Packet {
                        bytes: KEYFRAME_REQUEST.to_vec(),
                        expires: Time(u64::MAX),
                    };
                    match peers[who].submit(&request, true) {
                        Ok(()) => {
                            last_key_request = Some(elapsed);
                            continue;
                        }
                        Err(Error::Blocked | Error::NotReady) => {}
                        Err(error) => {
                            metrics.failed = Some(error);
                            break;
                        }
                    }
                }
                if who == 0
                    && let Some(packet) = queued.front()
                {
                    match peers[who].submit(packet, true) {
                        Ok(()) => {
                            queued.accepted();
                            continue;
                        }
                        Err(Error::Blocked | Error::NotReady) => {}
                        Err(error) => {
                            metrics.failed = Some(error);
                            break;
                        }
                    }
                }
                settled = true;
                break;
            }
            if metrics.failed.is_some() {
                break;
            }
            assert!(settled, "video datagram continuation did not settle");
        }
        if metrics.failed.is_some() {
            break;
        }
    }
    metrics.decoder_rejected = decoder.rejected;
    metrics.admitted_pictures = queued.admitted_pictures;
    metrics.terminal_pictures = queued.retire();
    metrics.assert_source_accounted(&trace);
    metrics
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decode_gap_includes_startup_and_missing_tail_but_not_drain() {
        let mut metrics = VideoMetrics::default();
        assert_eq!(metrics.max_decode_gap(1000), 1000);
        metrics.decoded = BTreeMap::from([(0, 40), (1, 100), (2, 1200)]);
        assert_eq!(metrics.max_decode_gap(1000), 900);
    }

    #[test]
    fn native_udp_reference_and_quic_control_decode_real_media() {
        let trace = Trace::encode(6000, 1_400_000);
        let native = native_video(&trace, SCENARIOS[0], 1, true);
        assert!(
            native.native_encoded.unwrap() >= u64::try_from(trace.pictures.len() * 4 / 5).unwrap()
        );
        assert!(native.decoded.len() >= trace.pictures.len() * 4 / 5);
        assert_eq!(native.invalid_identity + native.timestamp_mismatches, 0);
        // Bound actual encoded load, not just the requested bitrate: this
        // fixed-source encoder disables skipping and can overshoot its target.
        // Retain changing identity pixels and texture above a flat lower field.
        let mut native_trace = Trace::captures(6000, INITIAL_BPS);
        let mut native_encoder = SourceEncoder::new(INITIAL_BPS, INITIAL_BPS, false);
        for id in 0..native_trace.pictures.len() {
            native_trace.pictures[id].raw[32 * WIDTH as usize..(WIDTH * HEIGHT) as usize].fill(128);
            assert!(native_trace.encode_picture(
                &mut native_encoder,
                id,
                INITIAL_BPS,
                f64::from(FRAME_RATE),
                id.is_multiple_of(60)
            ));
        }
        assert!(native_trace.bytes() * 8000 <= u64::from(INITIAL_BPS) * native_trace.duration);
        let control = native_video(&native_trace, SCENARIOS[0], 1, false);
        assert_eq!(control.source_encoded.len(), native_trace.pictures.len());
        assert_eq!(
            control.native_encoded.unwrap(),
            u64::try_from(native_trace.pictures.len()).unwrap()
        );
        assert_eq!(control.decoded.len(), native_trace.pictures.len());
        assert_eq!(control.source_dropped, Some(0));
        assert_eq!(control.native_input_pending, Some(0));
        assert_eq!(control.failed, None);
        assert_eq!(control.duplicates, 0);
        assert_eq!(control.invalid_identity + control.timestamp_mismatches, 0);
        let quic = quic_video(&trace, SCENARIOS[0], 1, QuicCongestion::Cubic, false);
        assert_eq!(quic.decoded.len(), trace.pictures.len());
        assert_eq!(quic.invalid_identity + quic.timestamp_mismatches, 0);
        assert_eq!(quic.failed, None);
        let gcc = native_video_controller(
            &trace,
            SCENARIOS[0],
            1,
            NativeSource::Raw,
            NativeController::Gcc,
        );
        assert!(!gcc.decoded.is_empty());
        assert_eq!(gcc.native_scream_feedback, Some(false));
        assert_eq!(native.native_scream_feedback, Some(true));
        let udp = datagram_video_mode(&trace, SCENARIOS[0], 1, MediaPath::Udp, MediaSource::Fixed);
        assert_eq!(udp.decoded.len(), trace.pictures.len());
        assert_eq!(udp.invalid_identity + udp.timestamp_mismatches, 0);
        assert_eq!(udp.failed, None);
        for path in [
            MediaPath::Udp,
            MediaPath::Quic {
                cc: QuicCongestion::Cubic,
                overlay: true,
            },
            MediaPath::Quic {
                cc: QuicCongestion::Bbr3,
                overlay: true,
            },
        ] {
            let guided = datagram_video_mode(&trace, SCENARIOS[0], 1, path, MediaSource::Guided);
            guided.assert_source_accounted(&trace);
            assert_eq!(guided.source_targets.len(), trace.pictures.len());
            assert_eq!(guided.source_targets.first(), Some(&INITIAL_BPS));
            assert!(
                guided
                    .source_targets
                    .iter()
                    .any(|&target| target > INITIAL_BPS)
            );
            assert!(!guided.source_encoded.is_empty());
            assert!(!guided.decoded.is_empty());
            assert_eq!(guided.invalid_identity + guided.timestamp_mismatches, 0);
            assert_eq!(guided.expired_fragments, 0);
            assert_eq!(guided.dependency_discarded, 0);
            assert_eq!(guided.failed, None);
        }
    }
    #[test]
    fn completed_old_picture_is_never_submitted_after_a_newer_picture() {
        let mut trace = Trace::captures(120, 1_400_000);
        let mut encoder = SourceEncoder::new(1_400_000, 1_400_000, false);
        for id in 0..3 {
            assert!(trace.encode_picture(
                &mut encoder,
                id,
                1_400_000,
                f64::from(FRAME_RATE),
                id != 1
            ));
        }
        let mut metrics = VideoMetrics::default();
        let mut decoder = Decoder::new();
        for id in [2, 0] {
            for packet in fragments(&trace, id, 0) {
                metrics.receive(&trace, &packet.bytes, 100, &mut decoder);
            }
        }
        assert_eq!(decoder.last_input, Some(2));
        assert_eq!(metrics.reordered_encoded_discarded, 1);
    }
    #[test]
    fn unencoded_capture_gaps_are_not_missing_encoded_references() {
        let mut trace = Trace::captures(120, 1_400_000);
        let mut encoder = SourceEncoder::new(1_400_000, 1_400_000, false);
        let mut metrics = VideoMetrics::default();
        let mut decoder = Decoder::new();
        for id in [0, 2] {
            assert!(trace.encode_picture(
                &mut encoder,
                id,
                1_400_000,
                f64::from(FRAME_RATE),
                id == 0
            ));
            for packet in fragments(&trace, id, 0) {
                metrics.receive(&trace, &packet.bytes, 100, &mut decoder);
            }
        }
        assert_eq!(metrics.decoded.len(), 2);
        assert_eq!(metrics.dependency_discarded, 0);
        assert!(!decoder.waiting_for_key);
        assert_eq!(decoder.rejected, 0);
    }
    #[test]
    fn actual_codec_preserves_picture_identity_and_timestamp_without_ancillary_data() {
        let trace = Trace::encode(1000, 1_400_000);
        let mut decoder = Decoder::new();
        for id in 0..trace.pictures.len() {
            decoder.submit(&trace, id, false);
            let decoded = decoder.output.try_recv().unwrap();
            assert_eq!(
                decoded.rtp_timestamp,
                RTP_BASE + u32::try_from(id).unwrap() * RTP_STEP
            );
            assert_eq!(picture_id(&decoded), Some(id));
        }
        assert_eq!(decoder.rejected, 0);
    }
}
