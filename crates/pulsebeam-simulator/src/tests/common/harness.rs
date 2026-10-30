use super::media::FixtureEncoding;
pub use super::native_video::{Playback, PlaybackExpectation};
use super::provenance::FixturePublishers;
use crate::tests::common::client::{
    AudioReceiveLog, ClientObservation, DecodedVideoSizes, NativeTrafficBaseline,
    NativeTrafficStatistics, QualityVideoReference, RequestedVideo, SimClientBuilder,
    VideoReceiveLog, VideoReceiveStats,
};
use crate::tests::common::decoder::{
    MAX_AUDIO_REFERENCE_PEAK, MAX_REFERENCE_MEAN_ABSOLUTE, MAX_REFERENCE_PEAK,
};
use crate::tests::common::media::{KeyframeRecoveryProbe, VbrProfile};
use crate::tests::common::{
    DEFAULT_SIM_SHARDS, reserve_subnet, start_sfu_node_with_transport, subnet_ip, subnet_ip_v6,
};
use anyhow::Context;
use pulsebeam_agent_native::agent_core::{
    TopicMode, TopicNotification, TopicPublisher, TopicRegistrations, TopicSubscriber,
};
pub use pulsebeam_runtime::net::shaper::{Capacity, Loss, Reorder};
use pulsebeam_testdata::{QualityVideoLayer, QualityVideoSource};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
// Process-wide shimmed clock, not tokio's: turmoil virtualises `tokio::time::Instant` per host,
// so a stamp taken on the coordinator cannot be compared with one taken inside a participant.
// That mismatch reported every time-to-first-frame as ~5s regardless of what happened.
use std::time::Instant;

mod histories;
mod ordered_recovery;

#[derive(Clone, Copy, Debug)]
pub enum Role {
    Publisher,
    OwnedPublisher,
    Subscriber,
    /// Data-channel-only participant: no video tracks added.
    DataOnly,
}

#[derive(Clone, Debug)]
pub struct Participant {
    pub name: &'static str,
    pub role: Role,
    pub rids: Vec<&'static str>,
    pub quality_video: Option<(QualityVideoSource, QualityVideoLayer)>,
    /// Number of RecvOnly video slots (for multi-subscriber participants). Default 1.
    pub slots: usize,
    pub starts_disconnected: bool,
    /// Publish with a variable-bitrate source (screen sharing) instead of constant-rate.
    pub vbr: Option<VbrProfile>,
    /// Also receive video. A real conference participant both sends and receives; the plain
    /// `publisher`/`subscriber` constructors keep the one-way shape used by older tests.
    pub subscribes: bool,
    /// Whether a receiving participant subscribes to newly discovered tracks automatically.
    pub auto_subscribe: bool,
    /// How many speakers this participant can receive at once. Zero receives no audio.
    ///
    /// The receiving end of the SFU's speaker selection: it forwards the loudest few, and this is
    /// how many "few" is for this listener.
    pub audio_slots: usize,
    /// Publish audio at this loudness, in negative dBov. `None` publishes no audio.
    ///
    /// Separate from the role: a conference participant sends audio *and* video, and the audio
    /// path is selected independently of the video one.
    pub audio_level_dbov: Option<i8>,
    /// Where in its speech cycle this speaker starts, in 20ms packets.
    pub audio_phase_offset: u64,
    /// Publish a synthetic temporal Dependency Descriptor with this many layers,
    /// so the SFU can exercise decode-target shedding.
    pub temporal_dd: Option<u8>,
    pub video_receive_mode: pulsebeam_agent_native::ReceiveMode,
    pub audio_receive_mode: pulsebeam_agent_native::ReceiveMode,
    pub audio_input: pulsebeam_agent_native::AudioInput,
    /// Make the published payload opaque (simulating SFrame/E2EE), forcing the SFU
    /// to forward on the Dependency Descriptor alone.
    pub opaque_payload: bool,
    pub corrupt_video_payload: bool,
    pub native_playback_only: bool,
    pub solid_picture: Option<Arc<super::media::SolidVideoInput>>,
    pub native_video_fixture: Option<Arc<super::media::NativeVideoFixture>>,
    pub corrupt_audio_payload: bool,
    pub suppress_natural_keyframe_repeats: bool,
    /// Model a legacy peer that never negotiates the Dependency Descriptor
    /// extension, exercising the marker/deep-inspection fallback for mixed rooms.
    pub marker_only: bool,
}

impl Participant {
    pub fn owned_publisher(name: &'static str) -> Self {
        let mut participant = Self::single_publisher(name);
        participant.role = Role::OwnedPublisher;
        participant
    }

    pub fn publisher(name: &'static str, rids: &[&'static str]) -> Self {
        Self {
            name,
            role: Role::Publisher,
            rids: rids.to_vec(),
            native_video_fixture: None,
            video_receive_mode: pulsebeam_agent_native::ReceiveMode::Encoded,
            audio_receive_mode: pulsebeam_agent_native::ReceiveMode::Decoded,
            audio_input: pulsebeam_agent_native::AudioInput::Pcm,
            quality_video: None,
            slots: 0,
            starts_disconnected: false,
            vbr: None,
            subscribes: false,
            auto_subscribe: true,
            temporal_dd: None,
            audio_level_dbov: None,
            audio_phase_offset: 0,
            audio_slots: 0,
            opaque_payload: false,
            corrupt_video_payload: false,
            native_playback_only: false,
            solid_picture: None,
            corrupt_audio_payload: false,
            suppress_natural_keyframe_repeats: false,
            marker_only: false,
        }
    }

    pub fn single_publisher(name: &'static str) -> Self {
        Self {
            name,
            native_video_fixture: None,
            role: Role::Publisher,
            video_receive_mode: pulsebeam_agent_native::ReceiveMode::Encoded,
            audio_receive_mode: pulsebeam_agent_native::ReceiveMode::Decoded,
            audio_input: pulsebeam_agent_native::AudioInput::Pcm,
            rids: Vec::new(),
            quality_video: None,
            slots: 0,
            starts_disconnected: false,
            vbr: None,
            subscribes: false,
            auto_subscribe: true,
            temporal_dd: None,
            audio_level_dbov: None,
            audio_phase_offset: 0,
            audio_slots: 0,
            opaque_payload: false,
            corrupt_video_payload: false,
            native_playback_only: false,
            solid_picture: None,
            corrupt_audio_payload: false,
            suppress_natural_keyframe_repeats: false,
            marker_only: false,
        }
    }

    pub fn quality_publisher(name: &'static str, source: QualityVideoSource) -> Self {
        Self::quality_publisher_at(name, source, QualityVideoLayer::P180)
    }

    /// Genuine distinct-content q/h/f inputs at their actual corpus dimensions.
    pub fn quality_ladder_publisher(name: &'static str, source: QualityVideoSource) -> Self {
        Self {
            quality_video: Some((source, QualityVideoLayer::P720)),
            ..Self::publisher(name, &["q", "h", "f"])
        }
    }

    pub fn quality_publisher_at(
        name: &'static str,
        source: QualityVideoSource,
        layer: QualityVideoLayer,
    ) -> Self {
        Self {
            quality_video: Some((source, layer)),
            ..Self::single_publisher(name)
        }
    }

    pub fn subscriber(name: &'static str) -> Self {
        Self {
            name,
            native_video_fixture: None,
            role: Role::Subscriber,
            video_receive_mode: pulsebeam_agent_native::ReceiveMode::Encoded,
            audio_receive_mode: pulsebeam_agent_native::ReceiveMode::Decoded,
            audio_input: pulsebeam_agent_native::AudioInput::Pcm,
            rids: Vec::new(),
            quality_video: None,
            slots: 1,
            starts_disconnected: false,
            vbr: None,
            subscribes: false,
            auto_subscribe: true,
            temporal_dd: None,
            audio_level_dbov: None,
            audio_phase_offset: 0,
            audio_slots: 0,
            opaque_payload: false,
            corrupt_video_payload: false,
            native_playback_only: false,
            solid_picture: None,
            corrupt_audio_payload: false,
            suppress_natural_keyframe_repeats: false,
            marker_only: false,
        }
    }

    /// Subscriber that opens `slots` RecvOnly video tracks.
    pub fn multi_subscriber(name: &'static str, slots: usize) -> Self {
        Self {
            name,
            native_video_fixture: None,
            role: Role::Subscriber,
            video_receive_mode: pulsebeam_agent_native::ReceiveMode::Encoded,
            audio_receive_mode: pulsebeam_agent_native::ReceiveMode::Decoded,
            audio_input: pulsebeam_agent_native::AudioInput::Pcm,
            rids: Vec::new(),
            quality_video: None,
            slots,
            starts_disconnected: false,
            vbr: None,
            subscribes: false,
            auto_subscribe: true,
            temporal_dd: None,
            audio_level_dbov: None,
            audio_phase_offset: 0,
            audio_slots: 0,
            opaque_payload: false,
            corrupt_video_payload: false,
            native_playback_only: false,
            solid_picture: None,
            corrupt_audio_payload: false,
            suppress_natural_keyframe_repeats: false,
            marker_only: false,
        }
    }

    /// Subscriber whose tracks are bound only by explicit subscription steps.
    pub fn manual_subscriber(name: &'static str, slots: usize) -> Self {
        Self {
            auto_subscribe: false,
            ..Self::multi_subscriber(name, slots)
        }
    }

    /// Data-channel-only participant: connects but adds no video tracks.
    pub fn data_participant(name: &'static str) -> Self {
        Self {
            name,
            native_video_fixture: None,
            role: Role::DataOnly,
            video_receive_mode: pulsebeam_agent_native::ReceiveMode::Encoded,
            audio_receive_mode: pulsebeam_agent_native::ReceiveMode::Decoded,
            audio_input: pulsebeam_agent_native::AudioInput::Pcm,
            rids: Vec::new(),
            quality_video: None,
            slots: 0,
            starts_disconnected: false,
            vbr: None,
            subscribes: false,
            auto_subscribe: true,
            temporal_dd: None,
            audio_level_dbov: None,
            audio_phase_offset: 0,
            audio_slots: 0,
            opaque_payload: false,
            corrupt_video_payload: false,
            native_playback_only: false,
            solid_picture: None,
            corrupt_audio_payload: false,
            suppress_natural_keyframe_repeats: false,
            marker_only: false,
        }
    }

    /// A publisher whose content is screen sharing: strongly variable bitrate, long static
    /// stretches. This is the case that exercises str0m's probe controller, because the sender
    /// sits in ALR whenever the screen is still.
    /// A screen share configured the way the client configures one.
    ///
    /// `VIDEO_PRESETS.detail`: a single 2.5 Mbps layer. The full ladder is negotiated but only
    /// `f` is sent, so there is no rung to fall back to and the stream pauses outright rather
    /// than degrading.
    ///
    /// The rate is what makes this worth modelling separately from a camera. At 2.5 Mbps it costs
    /// twice the camera's top layer, so a viewer watching both sits close to the edge of most
    /// links - and that is the regime where allocation decisions are finely balanced. Every
    /// simulated publisher previously topped out at 1.25 Mbps, which put that regime out of
    /// reach: a production flap between the two streams was invisible here for want of a source
    /// that cost what the real one costs.
    pub fn screensharer(name: &'static str) -> Self {
        Self {
            vbr: Some(VbrProfile::screenshare_detail()),
            ..Self::publisher(name, &["f"])
        }
    }

    /// A screen sharer that goes genuinely still between bursts: long enough for the SFU to mark
    /// the layer dead and for the pacer's RTX cache to drain. See
    /// [`VbrProfile::screenshare_static`].
    pub fn static_screensharer(name: &'static str) -> Self {
        Self {
            vbr: Some(VbrProfile::screenshare_static()),
            ..Self::publisher(name, &["f"])
        }
    }

    /// Also receive video, making this a full two-way conference participant.
    pub fn and_subscribes(mut self) -> Self {
        self.subscribes = true;
        self.slots = self.slots.max(1);
        self
    }

    /// Reserve `slots` receive transceivers for audio.
    ///
    /// Not a cap on who the SFU forwards: how many speakers are selected is a property of the
    /// room, so a plan that wants contention needs more speakers than the room has slots, not a
    /// listener asking for fewer.
    pub fn hearing(mut self, slots: usize) -> Self {
        self.audio_slots = slots;
        self
    }

    /// Also publish audio, at the given loudness in negative dBov.
    ///
    /// Around -30 is someone talking; below about -60 is a quiet room. The SFU ranks speakers by
    /// this and forwards only the loudest few, so the value decides who gets heard.
    pub fn speaking_at(mut self, level_dbov: i8) -> Self {
        self.audio_level_dbov = Some(level_dbov);
        self
    }

    /// Talk in turn with another speaker rather than over them.
    ///
    /// Two sources at the same loudness and the same phase talk simultaneously forever, so the
    /// selector ranks them once and never switches. Offsetting one is what makes a slot change
    /// hands, and slot stealing is where audio is hardest.
    pub fn taking_turns_after(mut self, packets: u64) -> Self {
        self.audio_phase_offset = packets;
        self
    }

    pub fn starts_disconnected(mut self) -> Self {
        self.starts_disconnected = true;
        self
    }

    /// Publish a synthetic L1T{layers} temporal Dependency Descriptor so the SFU
    /// can shed decode targets.
    pub fn with_native_pcm_audio(mut self) -> Self {
        self.audio_input = pulsebeam_agent_native::AudioInput::Pcm;
        self
    }

    pub fn with_native_decoded_audio(mut self) -> Self {
        self.audio_receive_mode = pulsebeam_agent_native::ReceiveMode::Decoded;
        self
    }

    pub fn with_native_decoded_video(mut self) -> Self {
        self.video_receive_mode = pulsebeam_agent_native::ReceiveMode::Decoded;
        self
    }

    pub fn with_encoded_and_native_decoded_video(mut self) -> Self {
        self.video_receive_mode = pulsebeam_agent_native::ReceiveMode::EncodedAndDecoded;
        self
    }

    pub fn with_native_video_fixture(
        mut self,
        fixture: Arc<super::media::NativeVideoFixture>,
    ) -> Self {
        self.temporal_dd = Some(fixture.temporal_layers());
        self.native_video_fixture = Some(fixture);
        self
    }

    pub fn with_temporal_dd(mut self, layers: u8) -> Self {
        self.temporal_dd = Some(layers);
        self
    }

    /// Never negotiate the Dependency Descriptor: a legacy marker/deep-inspection
    /// peer. Used to test mixed rooms (DD on one side, marker-only on the other).
    pub fn marker_only(mut self) -> Self {
        self.marker_only = true;
        self
    }

    /// End-user native playback: encoded samples authenticate origin only.
    /// No independent decoder or observer-driven keyframe retry controls RTC.
    pub fn with_solid_video(mut self, picture: Arc<super::media::SolidVideoInput>) -> Self {
        self.solid_picture = Some(picture);
        self.rids = vec!["q"];
        self.vbr = None;
        self.temporal_dd = None;
        self.quality_video = None;
        self
    }

    pub fn with_native_playback(mut self) -> Self {
        self.video_receive_mode = pulsebeam_agent_native::ReceiveMode::EncodedAndDecoded;
        self.native_playback_only = true;
        self
    }

    pub fn with_corrupt_video_payload(mut self) -> Self {
        self.corrupt_video_payload = true;
        self
    }

    pub fn with_encoded_audio(mut self) -> Self {
        self.audio_receive_mode = pulsebeam_agent_native::ReceiveMode::Encoded;
        self
    }

    pub fn with_corrupt_audio_payload(mut self) -> Self {
        self.audio_receive_mode = pulsebeam_agent_native::ReceiveMode::Encoded;
        self.corrupt_audio_payload = true;
        self
    }

    pub fn suppress_natural_keyframe_repeats(mut self) -> Self {
        self.suppress_natural_keyframe_repeats = true;
        self
    }
}

pub struct Room {
    pub name: &'static str,
    pub participants: Vec<Participant>,
}

impl Room {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            participants: Vec::new(),
        }
    }

    pub fn with_participant(mut self, p: Participant) -> Self {
        self.participants.push(p);
        self
    }
}

/// Builder for video quality assertions. Use the named `allow_*` methods to
/// relax specific constraints from their zero defaults.
#[derive(Clone, Debug)]
pub struct VideoQuality {
    pub min_frames: u64,
    pub min_keyframes: u64,
    pub max_decoder_errors: u64,
    pub max_decoder_error_keyframes: u64,
    pub max_missing_parameter_set_keyframes: u64,
    pub max_non_contiguous: u64,
    pub native_temporal_indices: &'static [i32],
}

impl VideoQuality {
    /// Require at least `n` frames. All other constraints default to zero.
    pub fn min_frames(n: u64) -> Self {
        Self {
            min_frames: n,
            min_keyframes: 0,
            max_decoder_errors: 0,
            max_decoder_error_keyframes: 0,
            max_missing_parameter_set_keyframes: 0,
            max_non_contiguous: 0,
            native_temporal_indices: &[],
        }
    }

    pub fn require_native_temporal_indices(mut self, indices: &'static [i32]) -> Self {
        self.native_temporal_indices = indices;
        self
    }

    pub fn min_keyframes(mut self, n: u64) -> Self {
        self.min_keyframes = n;
        self
    }

    /// Allow up to `n` keyframes whose complete access unit lacked parameter sets.
    #[allow(dead_code)]
    pub fn allow_keyframe_parameter_set_losses(mut self, n: u64) -> Self {
        self.max_missing_parameter_set_keyframes = n;
        self
    }

    /// Allow up to `n` sequence-number gaps. One gap is normal per event that
    /// breaks the egress sequence: a simulcast switch, or a pause/resume such as
    /// a publisher partition, reconnect, or abrupt exit.
    pub fn allow_gaps(mut self, n: u64) -> Self {
        self.max_non_contiguous = n;
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SourceInputTier {
    Full,
    T0T1,
}

#[allow(dead_code)]
pub enum Step {
    IntentHistory {
        history: crate::tests::histories::History,
        witness: Arc<Mutex<crate::tests::histories::Witness>>,
    },
    StopServer,
    RestartServer,
    WaitForTopicPublisher {
        participant: &'static str,
        topic: &'static str,
        mode: TopicMode,
        next_sequence: u64,
    },
    OrderedTopicRecovery {
        publisher: &'static str,
        topic: &'static str,
        recovering: [&'static str; 2],
        late: &'static str,
    },
    WaitForDataSequence {
        participant: &'static str,
        topic: &'static str,
        expected: &'static [&'static [u8]],
    },
    // ── Time ──────────────────────────────────────────────────────────────
    /// Advance simulated time. All participants run during this window.
    /// Also snapshots TX/RX byte counters for subsequent `*BytesInterval` checks.
    Run {
        description: &'static str,
        duration: Duration,
    },
    /// Wait in simulated time until every same-room video observer knows this publication.
    /// Does not reset the measurement window; fails if discovery exceeds the bounded budget.
    WaitForPublication {
        description: &'static str,
        publisher: &'static str,
        timeout: Duration,
    },
    /// Start a fresh measurement and wait for allocation plus newly decoded video
    /// from every named origin. Does not replace a sustained observation window.
    WaitForVideo {
        description: &'static str,
        participant: &'static str,
        publishers: &'static [&'static str],
        timeout: Duration,
    },
    /// Require fresh source-specific independent decode at the supplied fixture size.
    /// This is encoded delivery evidence, not native playout or an allocation rank.
    WaitForVideoAtResolution {
        description: &'static str,
        participant: &'static str,
        publishers: &'static [&'static str],
        min_resolution: (usize, usize),
        timeout: Duration,
    },
    BeginControllerStall {
        duration: Duration,
    },
    WaitForControllerStallEnd,
    ReconnectThroughForeignShard {
        description: &'static str,
        participant: &'static str,
    },
    FailNextMaterialization {
        description: &'static str,
    },

    // ── Network ───────────────────────────────────────────────────────────
    Partition {
        description: &'static str,
        from: &'static str,
        to: &'static str,
    },
    Repair {
        description: &'static str,
        from: &'static str,
        to: &'static str,
    },
    Hold {
        description: &'static str,
        from: &'static str,
        to: &'static str,
    },
    Release {
        description: &'static str,
        from: &'static str,
        to: &'static str,
    },

    // ── Participant lifecycle ──────────────────────────────────────────────
    /// Connect a participant declared with `.starts_disconnected()`, or
    /// a previously disconnected one. Same as Reconnect semantically.
    Join {
        description: &'static str,
        participant: &'static str,
    },
    /// Abort on the first public admission failure instead of retrying automatically.
    JoinOnce {
        description: &'static str,
        participant: &'static str,
    },
    /// Gracefully disconnect: sends HTTP DELETE + RTC disconnect.
    Disconnect {
        description: &'static str,
        participant: &'static str,
    },
    /// Drop without signaling — models a process kill / abrupt exit.
    AbruptExit {
        description: &'static str,
        participant: &'static str,
    },
    /// Reconnect after a previous Disconnect or AbruptExit.
    Reconnect {
        description: &'static str,
        participant: &'static str,
    },
    BeginHeldReconnect {
        description: &'static str,
        participant: &'static str,
        timeout: Duration,
    },
    ReleaseHeldReconnect {
        description: &'static str,
        participant: &'static str,
        held_for: Duration,
    },
    WaitSinceBootstrapRelease {
        description: &'static str,
        participant: &'static str,
        duration: Duration,
    },

    // ── Subscriptions ─────────────────────────────────────────────────────
    /// Call driver.set_subscriptions(). Processed on next drive tick.
    SetSubscriptions {
        description: &'static str,
        participant: &'static str,
        subscriptions: Vec<VideoSubscription>,
    },
    RelabelOwnedVideo {
        description: &'static str,
        participant: &'static str,
        label: &'static str,
    },
    /// Subscribe to specific participants by name, at the given heights.
    ///
    /// Prefer this over [`Step::SubscribeAll`] whenever *which* track gets which height matters.
    /// `SubscribeAll` applies heights round-robin over tracks sorted by runtime participant id,
    /// which is generated per run - so `heights: &[720, 0]` hides an arbitrary participant rather
    /// than a chosen one, and a test written that way is not reproducible.
    SubscribeTo {
        description: &'static str,
        participant: &'static str,
        /// `(publisher name, target height)`. Height `0` hides that stream.
        targets: &'static [(&'static str, u32)],
    },
    /// Subscribe to specific participants with the QoS fields that affect allocator contention.
    /// Each tuple is `(publisher name, target height, minimum height, priority)`.
    SubscribeToQos {
        description: &'static str,
        participant: &'static str,
        targets: &'static [(&'static str, u32, u32, u32)],
    },
    /// Configured requests use plan names; the driver submits resolved public IDs.
    SubscribeToConfigured {
        description: &'static str,
        participant: &'static str,
        targets: Vec<VideoSubscription>,
    },
    /// Subscribe the participant to ALL currently discovered video tracks.
    /// `heights` is applied round-robin to discovered tracks (sorted ascending).
    /// Example: heights=&[720, 180] with 2 tracks → track[0]@720, track[1]@180.
    SubscribeAll {
        description: &'static str,
        participant: &'static str,
        heights: &'static [u32],
    },

    // ── Data channels ─────────────────────────────────────────────────────
    /// A named speaker currently holds one of this subscriber's audio slots.
    ///
    /// Requires both the current client catalog binding and the actual owned SFU assignment.
    /// Neither cumulative queueing nor receiver-index order proves this current pin claim.
    CheckSpeakerHeld {
        description: &'static str,
        participant: &'static str,
        speaker: &'static str,
    },

    /// Set this participant's audio policy.
    ///
    /// `pinned` names participants for readability; the harness resolves each to that
    /// participant's audio track ids, because the wire pins tracks - one participant can publish
    /// a microphone and a screen share's audio, and pinning one must not pin the other.
    ///
    /// `auto` fills the slots pinning does not claim by loudness, which is the default a client
    /// that never calls this gets.
    SetAudioIntent {
        description: &'static str,
        participant: &'static str,
        pinned: &'static [&'static str],
        auto: bool,
    },

    DeclarePublishTopic {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        mode: TopicMode,
    },
    DeclareSubscribeTopic {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        mode: TopicMode,
    },
    PublishData {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        mode: TopicMode,
        data: &'static [u8],
    },

    /// Explicit measurement boundary, not a wait. Only an intentional absence
    /// may exclude the gap before the first resumed frame.
    BeginNativePlayback {
        description: &'static str,
        participant: &'static str,
        after_pause: bool,
    },
    UpdateSolidVideo {
        description: &'static str,
        participant: &'static str,
    },
    QuietSolidVideo {
        description: &'static str,
        participant: &'static str,
    },
    /// Actual supplied capture and native pixels, including uniform content.
    WaitForSourceVideo {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
        min_resolution: (u32, u32),
        timeout: Duration,
    },
    /// Complete fixed-full-rate input interval, followed only by its event deadlines.
    MeasureSourceWindow {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
        encodings: &'static [&'static str],
        duration: Duration,
    },
    /// Bounded first-visible-output contract, anchored at this step's start.
    WaitForNativeVideo {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
        min_resolution: (u32, u32),
        timeout: Duration,
    },
    CheckNativePlayback {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
        expected: PlaybackExpectation,
        same_receiver: bool,
    },

    // ── Assertions ─────────────────────────────────────────────────────────
    /// Deep QoE check: frames are renderable, SPS/PPS present before keyframes,
    /// no backward timestamp jumps, bounded sequence gaps.
    CheckNativeDecodedVideo {
        description: &'static str,
        participant: &'static str,
        dimensions: (u32, u32),
        min_fresh_frames: u64,
    },
    /// Admission-specific actual native I420 progress, separate from the
    /// encoded observer and its independent H264 decoder.
    CheckNativeVideoDecodedFromInterval {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
        min_frames: u64,
        min_resolution: (u32, u32),
    },
    /// Source-authenticated native progress over the entire last Run. This is
    /// not a cadence, deadline or all-frame fidelity assertion under contention.
    CheckNativeSourceProgress {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
        min_frames: u64,
        min_resolution: (u32, u32),
    },
    /// Hard input-only premise for the controlled T3 policy witnesses.
    CheckSourceInputLoad {
        description: &'static str,
        participant: &'static str,
        protected: &'static [(&'static str, &'static str)],
        competing: &'static [(&'static str, &'static str)],
        competing_tier: SourceInputTier,
        capacity_bps: u64,
        max_backlog: Duration,
    },
    CheckVideoQuality {
        description: &'static str,
        participant: &'static str,
        quality: VideoQuality,
    },
    /// Video quality over the last `Step::Run` only, rather than the whole call.
    ///
    /// What a cumulative count cannot express: a stream that worked, stopped, and never came
    /// back still has all its frames. Asking about the window after a disturbance is the only way
    /// to say "and then it recovered".
    CheckVideoQualityInterval {
        description: &'static str,
        participant: &'static str,
        quality: VideoQuality,
    },
    /// Assert this participant received no video frames from the named publisher.
    CheckVideoNotReceivedFrom {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
    },
    CheckVideoReceivedFrom {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
        min_frames: u64,
    },
    CheckVideoDecodedFrom {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
        min_frames: u64,
        min_resolution: (usize, usize),
    },
    CheckVideoDecodedPublication {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
        min_frames: u64,
    },
    /// Independently decoded video from this origin during the last measurement window.
    CheckVideoDecodedFromInterval {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
        min_frames: u64,
        min_resolution: (usize, usize),
    },
    CheckVideoNotRenderedFrom {
        description: &'static str,
        participant: &'static str,
        publisher: &'static str,
    },
    /// This participant is still the same participant it was - it reconnected, it did not rejoin.
    ///
    /// A reconnect keeps the participant id and changes only the connection generation. If the
    /// client comes back as a new participant instead, everyone else keeps a tile for the old one
    /// and gains a second for the new: one person, twice on screen.
    CheckIdentityStable {
        description: &'static str,
        participant: &'static str,
    },
    /// Exactly these participants are still known to the client - no more, no fewer.
    ///
    /// Catches the ghost: somebody who left the room but whose track the SFU keeps announcing, so
    /// the client keeps a tile, a name and a publication for a person who is not there. A test
    /// that only checks the living participants are present cannot see it.
    CheckParticipantsKnown {
        description: &'static str,
        participant: &'static str,
        expected: &'static [&'static str],
    },
    /// Assert the publisher has received at most `max` keyframe (PLI) requests.
    /// A constantly climbing count means downstream cannot decode the forwarded
    /// stream — the signature of a broken DD/reassembly path (the "PLI storm").
    CheckKeyframeRequests {
        description: &'static str,
        participant: &'static str,
        max: u64,
    },
    /// Assert the publisher received at least `min` keyframe requests.
    CheckKeyframeRequestsAtLeast {
        description: &'static str,
        participant: &'static str,
        min: u64,
    },
    RecoverVideoAfterLostKeyframe {
        description: &'static str,
        publisher: &'static str,
        subscriber: &'static str,
        encoding: &'static str,
    },
    CheckKeyframeRecovery {
        description: &'static str,
        publisher: &'static str,
        subscriber: &'static str,
        encoding: &'static str,
    },
    /// Settled application admission errors, not server materialization events.
    CheckAdmissionFailed {
        description: &'static str,
        participant: &'static str,
        expected: u64,
    },

    /// Assert the participant has an active peer connection.
    CheckConnected {
        description: &'static str,
        participant: &'static str,
    },
    /// Assert the participant has NO active peer connection.
    CheckNotConnected {
        description: &'static str,
        participant: &'static str,
    },
    /// Cumulative bytes received ≥ min_bytes.
    CheckRxBytes {
        description: &'static str,
        participant: &'static str,
        min_bytes: u64,
    },
    CheckNativeAudioDecoded {
        description: &'static str,
        participant: &'static str,
        min_samples: u64,
        min_receivers: usize,
        max_silence: Option<Duration>,
    },
    WaitForNativeAudioRecovery {
        description: &'static str,
        participant: &'static str,
        min_samples: u64,
        timeout: Duration,
    },
    CheckNativeAudioQuiet {
        description: &'static str,
        participant: &'static str,
        min_blocks: u64,
        timeout: Duration,
    },
    CheckNativeAudioSilent {
        description: &'static str,
        participant: &'static str,
    },
    /// Native encoded packets and independent decoder results, without publisher tags.
    CheckEncodedAudio {
        description: &'static str,
        participant: &'static str,
        corrupt: bool,
    },
    CheckEncodedAudioInterval {
        description: &'static str,
        participant: &'static str,
        corrupt: bool,
    },
    /// Publicly observed source assignments and the actual encoded receiver stream bound.
    CheckAudioSlotStreams {
        description: &'static str,
        participant: &'static str,
        expected_sources: &'static [&'static str],
        max_streams: usize,
    },
    CheckAudioCatalogFrom {
        description: &'static str,
        participant: &'static str,
        expected: &'static [&'static str],
    },
    /// Cumulative bytes sent ≥ min_bytes.
    CheckTxBytes {
        description: &'static str,
        participant: &'static str,
        min_bytes: u64,
    },
    /// Bytes received since the last Step::Run ≥ min_bytes (per-window rate check).
    CheckRxBytesInterval {
        description: &'static str,
        participant: &'static str,
        min_bytes: u64,
    },
    /// Change a participant's downlink capacity mid-plan.
    ///
    /// Models the link improving or degrading under a live call, which is what separates a
    /// congestion controller that re-discovers capacity from one that has talked itself into a
    /// corner it cannot probe out of.
    SetBandwidth {
        description: &'static str,
        participant: &'static str,
        bits_per_sec: u64,
    },
    /// Apply a capacity *schedule* — a ramp or an oscillation — rather than a step change.
    ///
    /// Real links do not change instantaneously. A controller that handles square waves but
    /// oscillates on a slow ramp is broken in a way no `SetBandwidth` plan can reveal.
    SetCapacity {
        description: &'static str,
        participant: &'static str,
        capacity: Capacity,
        max_backlog: Duration,
    },
    /// Configure UDP datagram loss for a participant's downlink.
    ///
    /// `LocalNodeSim` advertises only UDP by default, so this governs the
    /// selected RTC path. The simulator does not pretend that dropping bytes
    /// from a reliable TCP stream models segment loss.
    SetLoss {
        description: &'static str,
        participant: &'static str,
        loss: Loss,
    },
    /// Configure packet reordering on a participant's downlink.
    ///
    /// Distinct from loss and from jitter: a reordered packet is overtaken by its successors, so
    /// the receiver both sees disturbed inter-arrival spacing and counts an unfilled gap as lost.
    /// A controller meets all three on a real path and responds to them differently.
    SetReorder {
        description: &'static str,
        participant: &'static str,
        reorder: Reorder,
    },
    /// Assert a [`Property`] of the run just completed.
    ///
    /// Prefer this to the `Check*` byte-count steps. See [`Property`] for why.
    Expect {
        description: &'static str,
        participant: &'static str,
        property: Property,
    },
    /// Log every measurable property for a participant, asserting nothing.
    ///
    /// Two jobs. It is how a threshold gets chosen — observe what healthy looks like rather than
    /// guessing a number and calibrating it to whatever the code happens to do today, which is
    /// how the byte-count floors ended up pinned below the bug they were meant to catch. And run
    /// across a matrix of link profiles it is the scoreboard: one table showing a change's effect
    /// on every scenario at once, instead of discovering two days later that a fix for
    /// screenshare wrecked cellular.
    Report {
        description: &'static str,
        participant: &'static str,
    },
    /// Bytes received since the last Step::Run ≤ `max_bytes`.
    ///
    /// The counterpart to [`Step::CheckRxBytesInterval`]: asserts a subscription is not being
    /// *over*-served. A capped subscription that receives far more than its layer costs is
    /// paying for padding and probes aimed at bandwidth it asked not to be given.
    CheckMaxRxBytesInterval {
        description: &'static str,
        participant: &'static str,
        max_bytes: u64,
    },
    /// Bytes sent since the last Step::Run ≥ min_bytes (per-window rate check).
    CheckTxBytesInterval {
        description: &'static str,
        participant: &'static str,
        min_bytes: u64,
    },
    /// Assert ≥1 payload matching `expected` was received on the topic.
    CheckDataReceived {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        expected: &'static [u8],
    },
    /// Assert NO payload matching `excluded` was received on the topic.
    CheckDataNotReceived {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        excluded: &'static [u8],
    },
    /// Assert the exact number of payloads received on a topic.
    ///
    /// Presence-only checks cannot distinguish one correct fanout from a duplicate storm or a
    /// subscriber that received only the first of several intended messages.
    CheckDataCount {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        expected: usize,
    },
    CheckDataSequence {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        expected: &'static [&'static [u8]],
    },
    CheckTopicIdentity {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        mode: TopicMode,
        publishers: &'static [&'static str],
        gap: Option<(u64, u64)>,
    },
}

// ── Internal lifecycle commands ─────────────────────────────────────────────

enum ParticipantCmd {
    /// Graceful shutdown (HTTP DELETE + RTC disconnect), then wait for Reconnect.
    Shutdown,
    /// Drop the client without signaling (AbruptExit), then wait for Reconnect.
    Drop,
    /// Reconnect (after Shutdown or Drop).
    Reconnect,
    /// Test over — exit the participant loop.
    Done,
}

// ── Pending driver operations (queued by coordinator, drained on drive tick) ─

#[derive(Clone)]
enum PendingDriverOp {
    SetSubscriptions(Vec<VideoSubscription>),
    RelabelOwnedVideo(String),
    ReplaceVideoSource(Arc<AtomicBool>),
    ReleaseReplacedVideoSource(Arc<AtomicBool>),
    ApplicationAgent(Arc<Mutex<Option<pulsebeam_agent_native::Agent>>>),
    DeclarePublishTopic(String, TopicMode),
    DeclareSubscribeTopic(String, TopicMode),
    PublishData(String, TopicMode, Vec<u8>),
    /// (pinned participant ids, auto)
    SetAudioIntent(Vec<String>, bool),
    SetAudioRequest(Vec<String>, bool, Option<u16>, Arc<AtomicBool>),
    ProducerPressure(Arc<Mutex<Option<(usize, usize)>>>),
}

pub type VideoSubscription = RequestedVideo;

#[derive(Clone, Default)]
struct PlanIntent {
    video: Vec<VideoSubscription>,
    audio: Vec<String>,
    automatic_audio: bool,
    topics: TopicRegistrations,
}

struct OperationQueue {
    pending: Vec<(u64, PendingDriverOp)>,
    sequence: u64,
    intent: PlanIntent,
    initial_topics: TopicRegistrations,
}

impl OperationQueue {
    fn new(topics: TopicRegistrations) -> Self {
        Self {
            pending: Vec::new(),
            sequence: 0,
            intent: PlanIntent {
                automatic_audio: true,
                topics: topics.clone(),
                ..Default::default()
            },
            initial_topics: topics,
        }
    }

    fn restart(&mut self) {
        *self = Self::new(self.initial_topics.clone());
    }

    fn push(&mut self, operation: PendingDriverOp) -> u64 {
        self.sequence += 1;
        match &operation {
            PendingDriverOp::SetSubscriptions(video) => {
                self.intent.video.clone_from(video);
                self.pending
                    .retain(|(_, op)| !matches!(op, PendingDriverOp::SetSubscriptions(_)));
            }
            PendingDriverOp::SetAudioIntent(pinned, automatic)
            | PendingDriverOp::SetAudioRequest(pinned, automatic, _, _) => {
                self.intent.audio.clone_from(pinned);
                self.intent.automatic_audio = *automatic;
            }
            PendingDriverOp::DeclarePublishTopic(topic, mode) => {
                let registration = TopicPublisher {
                    topic: topic.clone(),
                    mode: *mode,
                };
                if !self.intent.topics.publishers.contains(&registration) {
                    self.intent.topics.publishers.push(registration);
                }
            }
            PendingDriverOp::DeclareSubscribeTopic(topic, mode) => {
                let registration = TopicSubscriber {
                    topic: topic.clone(),
                    mode: *mode,
                };
                if !self.intent.topics.subscribers.contains(&registration) {
                    self.intent.topics.subscribers.push(registration);
                }
            }
            PendingDriverOp::RelabelOwnedVideo(_)
            | PendingDriverOp::ReplaceVideoSource(_)
            | PendingDriverOp::ReleaseReplacedVideoSource(_)
            | PendingDriverOp::ApplicationAgent(_)
            | PendingDriverOp::ProducerPressure(_)
            | PendingDriverOp::PublishData(..) => {}
        }
        self.pending.push((self.sequence, operation));
        self.sequence
    }
}

#[test]
fn driver_restart_restores_initial_intent() {
    let mut queue = OperationQueue::new(TopicRegistrations::default());
    queue.push(PendingDriverOp::SetAudioIntent(
        vec!["speaker".into()],
        false,
    ));
    queue.push(PendingDriverOp::DeclarePublishTopic(
        "old-topic".into(),
        TopicMode::Latest,
    ));
    queue.restart();
    assert!(queue.pending.is_empty() && queue.intent.audio.is_empty());
    assert!(queue.intent.automatic_audio);
    assert_eq!(queue.intent.topics, TopicRegistrations::default());
    queue.push(PendingDriverOp::SetSubscriptions(vec![VideoSubscription {
        participant_id: "old".into(),
        height: 180,
        ..Default::default()
    }]));
    queue.push(PendingDriverOp::SetSubscriptions(Vec::new()));
    assert_eq!(
        queue.pending.len(),
        1,
        "an unresolved replacement must not overwrite its successor"
    );
    assert!(queue.intent.video.is_empty());
}

// ── Per-participant shared state ────────────────────────────────────────────

struct ParticipantShared {
    bootstrap_hold: super::client::BootstrapHold,
    driver_admission_retry: Mutex<bool>,
    failed_admissions: Mutex<u64>,
    observation: Mutex<Option<ClientObservation>>,
    interval_audio_sources: Mutex<BTreeSet<String>>,
    video_rx: Arc<Mutex<VideoReceiveLog>>,
    audio_rx: Arc<Mutex<AudioReceiveLog>>,
    quality_references: Arc<Mutex<BTreeMap<String, &'static QualityVideoReference>>>,
    h264_publishers: Arc<Mutex<FixturePublishers>>,
    solid_picture: Option<Arc<super::media::SolidVideoInput>>,
    statistics: Mutex<Option<NativeTrafficStatistics>>,
    interval_statistics: Mutex<NativeTrafficBaseline>,
    connected: Mutex<bool>,
    keyframe_recovery: Arc<Mutex<KeyframeRecoveryProbe>>,
    media_kinds: Mutex<HashMap<String, (bool, bool)>>,
    /// Every participant id this name has had, oldest first. All but the last are dead identities.
    incarnations: Mutex<Vec<String>>,
    /// Set to Some(...) once the participant has connected for the first time.
    participant_id: Mutex<Option<String>>,
    /// Operations queued by the coordinator; drained on next drive tick.
    pending_ops: Mutex<OperationQueue>,
    /// Data received per topic, accumulated across all drive ticks.
    topic_events: Mutex<HashMap<String, Vec<TopicNotification>>>,
    /// Remote video tracks discovered via signaling (track IDs).
    discovered_tracks: Mutex<HashSet<String>>,
    video_label: Mutex<String>,
}

impl ParticipantShared {
    fn record_observation(&self, observation: ClientObservation) -> anyhow::Result<()> {
        let mut sources = self.interval_audio_sources.lock().unwrap();
        for id in observation.snapshot.audio_mapping.values() {
            let publication = observation
                .snapshot
                .publications
                .get(id)
                .context("public audio assignment has no publication")?;
            anyhow::ensure!(
                publication.kind == pulsebeam_agent_native::agent_core::MediaKind::Audio,
                "public audio assignment has the wrong kind"
            );
            sources.insert(publication.participant_id.clone());
        }
        *self.observation.lock().unwrap() = Some(observation);
        Ok(())
    }

    fn record_statistics(&self, sample: Option<NativeTrafficStatistics>) -> anyhow::Result<()> {
        if let Some(stats) = &sample {
            let at = super::native::callback_clock()?.project(stats.timestamp_us)?;
            self.interval_statistics.lock().unwrap().observe(stats, at);
        }
        *self.statistics.lock().unwrap() = sample;
        Ok(())
    }

    fn new(
        quality_references: Arc<Mutex<BTreeMap<String, &'static QualityVideoReference>>>,
        h264_publishers: Arc<Mutex<FixturePublishers>>,
        solid_picture: Option<Arc<super::media::SolidVideoInput>>,
        topics: TopicRegistrations,
    ) -> Self {
        Self {
            bootstrap_hold: super::client::BootstrapHold::default(),
            driver_admission_retry: Mutex::new(false),
            failed_admissions: Mutex::new(0),
            observation: Mutex::new(None),
            interval_audio_sources: Mutex::new(BTreeSet::new()),
            video_rx: Arc::new(Mutex::new(VideoReceiveLog::default())),
            audio_rx: Arc::new(Mutex::new(AudioReceiveLog::default())),
            quality_references,
            h264_publishers,
            solid_picture,
            statistics: Mutex::new(None),
            interval_statistics: Mutex::new(NativeTrafficBaseline::default()),
            connected: Mutex::new(false),
            keyframe_recovery: Arc::default(),
            media_kinds: Mutex::new(HashMap::new()),
            incarnations: Mutex::new(Vec::new()),
            participant_id: Mutex::new(None),
            pending_ops: Mutex::new(OperationQueue::new(topics)),
            topic_events: Mutex::new(HashMap::new()),
            discovered_tracks: Mutex::new(HashSet::new()),
            video_label: Mutex::new("video".into()),
        }
    }
}

struct ParticipantHandle {
    shared: Arc<ParticipantShared>,
    cmd_tx: mpsc::Sender<ParticipantCmd>,
    room_name: &'static str,
    /// When this participant last asked for a stream. Time-to-first-frame is measured from here,
    /// not from the start of the plan: the settle steps before a subscription are not time the
    /// viewer spent waiting.
    subscribed_at: Option<Instant>,
    interval_video_baseline: VideoReceiveStats,
    interval_decoded_video_baseline: BTreeMap<String, u64>,
    interval_decoded_size_baseline: BTreeMap<String, DecodedVideoSizes>,
    native_video_baseline: super::native_video::NativeDecodedVideoLog,
    native_playback_track: Option<pulsebeam_agent_native::RemoteTrack>,
    native_playback_started: bool,
    native_origin_interval: Instant,
    native_video_stats_timestamp: Option<i64>,
    native_video_decoder_frames: Option<(u64, String, u64)>,
    /// Ground truth, from the plan rather than from anything the SFU said.
    ///
    /// The room invariant is checked against these: what this participant actually publishes, and
    /// whether it is currently in the call at all. A client's belief is only interesting next to
    /// something known to be true.
    publishes_video: bool,
    publishes_audio: bool,
    quality_video: Option<(QualityVideoSource, QualityVideoLayer)>,
    observes_video: bool,
    auto_subscribe: bool,
    video_capacity: usize,
    retirement_deadline: Option<Instant>,
    /// Whether the plan has this participant in the room right now.
    present: bool,
}

impl ParticipantHandle {
    fn send_command(&self, command: ParticipantCmd) {
        // Best-effort: a participant that has abruptly exited has dropped its
        // command receiver, so a send can legitimately fail during teardown
        // races. That is not a test failure.
        let _ = self.cmd_tx.try_send(command);
    }

    fn tx_bytes(&self) -> u64 {
        self.shared
            .statistics
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|stats| stats.bytes_sent)
            .expect("traffic assertion requires actual native bytesSent")
    }
    fn rx_bytes(&self) -> u64 {
        self.shared
            .statistics
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|stats| stats.bytes_received)
            .expect("traffic assertion requires actual native bytesReceived")
    }
    fn keyframe_requests(&self) -> u64 {
        self.shared
            .statistics
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|stats| stats.keyframe_requests)
            .expect("PLI assertion requires actual outbound-video native pliCount")
    }
    fn interval_delta(
        &self,
        field: fn(&NativeTrafficStatistics) -> Option<u64>,
    ) -> Result<u64, String> {
        let guard = self.shared.statistics.lock().unwrap();
        let current = guard
            .as_ref()
            .ok_or("native interval ending sample unavailable")?;
        let baseline = self.shared.interval_statistics.lock().unwrap();
        current.interval_delta(baseline.sample(), field)
    }
    fn interval_baseline(&self, field: fn(&NativeTrafficStatistics) -> Option<u64>) -> u64 {
        let guard = self.shared.statistics.lock().unwrap();
        let current = guard
            .as_ref()
            .expect("native interval ending sample unavailable");
        let baseline = self.shared.interval_statistics.lock().unwrap();
        let delta = current
            .interval_delta(baseline.sample(), field)
            .expect("traffic assertion requires a valid native interval");
        field(current).expect("validated native field") - delta
    }
    fn interval_tx_baseline(&self) -> u64 {
        self.interval_baseline(|stats| stats.bytes_sent)
    }
    fn interval_rx_baseline(&self) -> u64 {
        self.interval_baseline(|stats| stats.bytes_received)
    }

    /// What kinds of media this client believes `publisher_id` is sending.
    fn media_kinds_of(&self, publisher_id: &str) -> Option<(bool, bool)> {
        self.shared
            .media_kinds
            .lock()
            .unwrap()
            .get(publisher_id)
            .copied()
    }

    fn connected(&self) -> bool {
        *self.shared.connected.lock().unwrap()
    }
    fn video_rx(&self) -> VideoReceiveLog {
        self.shared.video_rx.lock().unwrap().clone()
    }

    fn audio_rx(&self) -> AudioReceiveLog {
        self.shared.audio_rx.lock().unwrap().clone()
    }

    fn video_stats_since_interval(&self) -> VideoReceiveStats {
        self.video_rx().stats().since(self.interval_video_baseline)
    }

    /// The SFU-side participant id, once connected. Keys the per-subscriber metrics.
    fn participant_id(&self) -> Option<String> {
        self.shared.participant_id.lock().unwrap().clone()
    }

    /// Media payload the SFU forwarded *to this participant* in the current window.
    fn forwarded_media(&self) -> u64 {
        self.participant_id()
            .map(|id| pulsebeam::sim_metrics::forwarded_media_bytes(&id))
            .unwrap_or(0)
    }

    fn snapshot_interval(&mut self) {
        self.shared.interval_audio_sources.lock().unwrap().clear();
        self.native_origin_interval = Instant::now();
        let sample = self.shared.statistics.lock().unwrap().clone();
        let generation = self
            .shared
            .observation
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|observation| observation.snapshot.generation)
            .map(pulsebeam_agent_native::agent_core::Generation::get);
        self.shared.interval_statistics.lock().unwrap().begin(
            sample,
            generation,
            self.native_origin_interval,
        );
        self.shared.audio_rx.lock().unwrap().begin_window();
        let mut video = self.shared.video_rx.lock().unwrap();
        video
            .native_origin
            .begin_progress(self.native_origin_interval);
        self.interval_video_baseline = video.stats();
        self.interval_decoded_video_baseline = video
            .decoded_by_publisher
            .iter()
            .map(|(publisher, decoded)| (publisher.clone(), decoded.frames))
            .collect();
        self.interval_decoded_size_baseline
            .clone_from(&video.decoded_sizes_by_publisher);
    }
}

// ── Participant task ────────────────────────────────────────────────────────

struct ParticipantTask {
    ip: IpAddr,
    server_ip: IpAddr,
    config: Participant,
    room_name: &'static str,
    tcp_only: bool,
    initial_topics: TopicRegistrations,
}

async fn run_participant(
    task: ParticipantTask,
    shared: Arc<ParticipantShared>,
    mut cmd_rx: mpsc::Receiver<ParticipantCmd>,
) -> anyhow::Result<()> {
    let ParticipantTask {
        ip,
        server_ip,
        config,
        room_name,
        tcp_only,
        initial_topics,
    } = task;
    if config.starts_disconnected {
        match cmd_rx.recv().await {
            Some(ParticipantCmd::Reconnect) => {}
            _ => return Ok(()),
        }
    }

    loop {
        if matches!(config.role, Role::OwnedPublisher) {
            *shared.video_label.lock().unwrap() = "video".into();
        }
        let mut builder = if tcp_only {
            SimClientBuilder::bind_tcp(ip, server_ip).await?
        } else {
            SimClientBuilder::bind(ip, server_ip).await?
        }
        .with_bootstrap_hold(shared.bootstrap_hold.clone())
        .with_quality_references(shared.quality_references.clone())
        .with_h264_publishers(shared.h264_publishers.clone())
        .with_video_receive_mode(config.video_receive_mode)
        .with_native_playback_only(config.native_playback_only)
        .with_audio_receive_mode(config.audio_receive_mode)
        .with_audio_input(config.audio_input)
        .with_audio_rx(shared.audio_rx.clone())
        .with_video_rx(shared.video_rx.clone())
        .with_keyframe_recovery_probe(shared.keyframe_recovery.clone())
        .with_initial_topics(initial_topics.clone())
        .with_driver_admission_retry(*shared.driver_admission_retry.lock().unwrap());

        if let Some(picture) = &config.solid_picture {
            builder = builder.with_solid_video(picture.clone());
        }
        if config.suppress_natural_keyframe_repeats {
            builder = builder.suppress_natural_keyframe_repeats();
        }
        if config.marker_only {
            builder = builder.without_dependency_descriptor();
        }
        match config.role {
            Role::Publisher | Role::OwnedPublisher => {
                let layers = (!config.rids.is_empty()).then(|| {
                    config
                        .rids
                        .iter()
                        .map(|rid| FixtureEncoding::new(*rid))
                        .collect()
                });
                builder = if matches!(config.role, Role::OwnedPublisher) {
                    builder.publish_owned_video("video")
                } else {
                    builder.publish_video(layers)
                };
                if let Some(fixture) = &config.native_video_fixture {
                    builder = builder.with_native_video_fixture(fixture.clone());
                }
                if let Some((source, layer)) = config.quality_video {
                    builder = builder.publish_quality_video(source, layer);
                } else {
                    if let Some(profile) = config.vbr {
                        builder = builder.with_vbr(profile);
                    }
                    if let Some(layers) = config.temporal_dd {
                        builder = builder.with_temporal_dd(layers);
                    }
                    if config.opaque_payload {
                        builder = builder.with_opaque_payload();
                    }
                    if config.corrupt_video_payload {
                        builder = builder.with_corrupt_video_payload();
                    }
                }
                if config.subscribes {
                    builder = builder.receive_video(config.slots.max(1));
                }
            }
            Role::Subscriber => {
                builder = builder.receive_video(config.slots.max(1));
                if config.corrupt_video_payload {
                    builder = builder.with_corrupt_video_payload();
                }
            }
            Role::DataOnly => {}
        }
        if let Some(level) = config.audio_level_dbov {
            builder = builder.publish_audio(level, config.audio_phase_offset);
        }
        if config.corrupt_audio_payload {
            builder = builder.with_corrupt_audio_payload();
        }
        if config.audio_slots > 0 {
            builder = builder.receive_audio(config.audio_slots);
        }
        if !config.auto_subscribe {
            builder = builder.manual_subscriptions();
        }

        let mut client = match builder.connect(room_name).await {
            Ok(client) => client,
            Err(error)
                if config.starts_disconnected || *shared.driver_admission_retry.lock().unwrap() =>
            {
                *shared.connected.lock().unwrap() = false;
                *shared.failed_admissions.lock().unwrap() += 1;
                tracing::debug!(%error, "participant materialization failed");
                match cmd_rx.recv().await {
                    Some(ParticipantCmd::Reconnect) => continue,
                    _ => break,
                }
            }
            Err(error) => return Err(error),
        };
        if let Some(id) = client.ctx.participant_id() {
            *shared.participant_id.lock().unwrap() = Some(id.clone());
            shared.incarnations.lock().unwrap().push(id);
        }
        *shared.connected.lock().unwrap() = true;

        let mut interval = tokio::time::interval(Duration::from_millis(10));
        let command = loop {
            tokio::select! {
                command = cmd_rx.recv() => break command,
                event = client.ctx.receive_event(), if !client.ctx.events_closed() => event?,
                _ = interval.tick() => {
                    let operations = std::mem::take(&mut shared.pending_ops.lock().unwrap().pending);
                    let mut retry = Vec::new();
                    for (sequence, operation) in operations {
                        let complete = match operation.clone() {
                            PendingDriverOp::SetSubscriptions(subscriptions) => client
                                .ctx
                                .set_video_subscriptions(subscriptions, sequence)
                                .await?,
                            PendingDriverOp::RelabelOwnedVideo(label) => {
                                let complete = client.ctx.relabel_owned_video(label.clone()).await?;
                                if complete { *shared.video_label.lock().unwrap() = label; }
                                complete
                            }
                            PendingDriverOp::ReplaceVideoSource(receipt) => {
                                let complete = client.replace_video_source().await?;
                                if complete { receipt.store(true, Ordering::Release); }
                                complete
                            }
                            PendingDriverOp::ReleaseReplacedVideoSource(receipt) => {
                                let complete = client.release_replaced_video_source().await?;
                                if complete { receipt.store(true, Ordering::Release); }
                                complete
                            }
                            PendingDriverOp::ApplicationAgent(receipt) => {
                                *receipt.lock().unwrap() = Some(client.ctx.agent.clone());
                                true
                            }
                            PendingDriverOp::DeclarePublishTopic(topic, mode) => {
                                client.ctx.register_publisher(topic, mode).await?
                            }
                            PendingDriverOp::DeclareSubscribeTopic(topic, mode) => {
                                client.ctx.register_subscriber(topic, mode).await?
                            }
                            PendingDriverOp::PublishData(topic, mode, data) => {
                                client.ctx.send_topic(&topic, mode, data).await?
                            }
                            PendingDriverOp::SetAudioIntent(participants, automatic) => {
                                client.ctx.set_audio_intent(&participants, automatic).await?
                            }
                            PendingDriverOp::SetAudioRequest(participants, automatic, delay, receipt) => {
                                let complete = client.ctx.set_audio_request(&participants, automatic, delay).await?;
                                if complete { receipt.store(true, Ordering::Release); }
                                complete
                            }
                            PendingDriverOp::ProducerPressure(receipt) => {
                                let settled = client.producer_pressure().await?;
                                if let Some(counts) = settled { *receipt.lock().unwrap() = Some(counts); }
                                settled.is_some()
                            }
                        };
                        if !complete {
                            retry.push((sequence, operation));
                        }
                        client.ctx.drain_events()?;
                    }
                    if !retry.is_empty() {
                        let mut pending = shared.pending_ops.lock().unwrap();
                        if pending.pending.iter().any(|(_, op)| matches!(op, PendingDriverOp::SetSubscriptions(_))) {
                            retry.retain(|(_, op)| !matches!(op, PendingDriverOp::SetSubscriptions(_)));
                        }
                        retry.append(&mut pending.pending);
                        pending.pending = retry;
                    }

                    client.tick().await?;
                    let observation = client.ctx.observation();
                    shared.record_observation(observation)?;
                    {
                        let mut received = shared.topic_events.lock().unwrap();
                        for event in client.ctx.topic_events.drain(..) {
                            let topic = match &event {
                                TopicNotification::Message(message) => &message.topic,
                                TopicNotification::RecoveryGap { subscriber, .. } => &subscriber.topic,
                                _ => unreachable!("client observation only retains application events"),
                            };
                            received.entry(topic.clone()).or_default().push(event);
                        }
                    }
                    *shared.discovered_tracks.lock().unwrap() = client.ctx.discovered_tracks.clone();
                    *shared.media_kinds.lock().unwrap() = client.ctx.media_kinds.clone();
                    shared.record_statistics(client.ctx.statistics()?)?;
                    *shared.connected.lock().unwrap() = client.ctx.connected();
                }
            }
        };

        shared.record_statistics(client.ctx.statistics()?)?;
        *shared.connected.lock().unwrap() = false;
        match command {
            None | Some(ParticipantCmd::Done) => {
                client.abort().await?;
                break;
            }
            Some(ParticipantCmd::Shutdown) => {
                client.close().await?;
                match cmd_rx.recv().await {
                    Some(ParticipantCmd::Reconnect) => continue,
                    _ => break,
                }
            }
            Some(ParticipantCmd::Drop) => {
                client.abort().await?;
                match cmd_rx.recv().await {
                    Some(ParticipantCmd::Reconnect) => continue,
                    _ => break,
                }
            }
            Some(ParticipantCmd::Reconnect) => {
                client.abort().await?;
                continue;
            }
        }
    }
    Ok(())
}

// ── Coordinator task ────────────────────────────────────────────────────────

fn step_name(step: &Step) -> &'static str {
    match step {
        Step::IntentHistory { .. } => "IntentHistory",
        Step::Run { .. } => "Run",
        Step::WaitForPublication { .. } => "WaitForPublication",
        Step::WaitForVideo { .. } => "WaitForVideo",
        Step::WaitForVideoAtResolution { .. } => "WaitForVideoAtResolution",
        Step::BeginControllerStall { .. } => "BeginControllerStall",
        Step::WaitForControllerStallEnd => "WaitForControllerStallEnd",
        Step::ReconnectThroughForeignShard { .. } => "ReconnectThroughForeignShard",
        Step::FailNextMaterialization { .. } => "FailNextMaterialization",
        Step::Partition { .. } => "Partition",
        Step::Repair { .. } => "Repair",
        Step::Hold { .. } => "Hold",
        Step::Release { .. } => "Release",
        Step::Join { .. } => "Join",
        Step::JoinOnce { .. } => "JoinOnce",
        Step::Disconnect { .. } => "Disconnect",
        Step::AbruptExit { .. } => "AbruptExit",
        Step::Reconnect { .. } => "Reconnect",
        Step::BeginHeldReconnect { .. } => "BeginHeldReconnect",
        Step::ReleaseHeldReconnect { .. } => "ReleaseHeldReconnect",
        Step::WaitSinceBootstrapRelease { .. } => "WaitSinceBootstrapRelease",
        Step::SetSubscriptions { .. } => "SetSubscriptions",
        Step::RelabelOwnedVideo { .. } => "RelabelOwnedVideo",
        Step::SubscribeAll { .. } => "SubscribeAll",
        Step::SubscribeTo { .. } => "SubscribeTo",
        Step::SubscribeToQos { .. } => "SubscribeToQos",
        Step::SubscribeToConfigured { .. } => "SubscribeToConfigured",
        Step::WaitForSourceVideo { .. } => "WaitForSourceVideo",
        Step::UpdateSolidVideo { .. } => "UpdateSolidVideo",
        Step::QuietSolidVideo { .. } => "QuietSolidVideo",
        Step::MeasureSourceWindow { .. } => "MeasureSourceWindow",
        Step::DeclarePublishTopic { .. } => "DeclarePublishTopic",
        Step::DeclareSubscribeTopic { .. } => "DeclareSubscribeTopic",
        Step::PublishData { .. } => "PublishData",
        Step::BeginNativePlayback { .. } => "BeginNativePlayback",
        Step::WaitForNativeVideo { .. } => "WaitForNativeVideo",
        Step::CheckNativePlayback { .. } => "CheckNativePlayback",
        Step::CheckNativeDecodedVideo { .. } => "CheckNativeDecodedVideo",
        Step::CheckNativeVideoDecodedFromInterval { .. } => "CheckNativeVideoDecodedFromInterval",
        Step::CheckNativeSourceProgress { .. } => "CheckNativeSourceProgress",
        Step::CheckSourceInputLoad { .. } => "CheckSourceInputLoad",
        Step::CheckVideoQuality { .. } => "CheckVideoQuality",
        Step::CheckVideoQualityInterval { .. } => "CheckVideoQualityInterval",
        Step::CheckVideoNotReceivedFrom { .. } => "CheckVideoNotReceivedFrom",
        Step::CheckVideoReceivedFrom { .. } => "CheckVideoReceivedFrom",
        Step::CheckVideoDecodedFrom { .. } => "CheckVideoDecodedFrom",
        Step::CheckVideoDecodedPublication { .. } => "CheckVideoDecodedPublication",
        Step::CheckVideoDecodedFromInterval { .. } => "CheckVideoDecodedFromInterval",
        Step::CheckVideoNotRenderedFrom { .. } => "CheckVideoNotRenderedFrom",
        Step::CheckKeyframeRequests { .. } => "CheckKeyframeRequests",
        Step::CheckKeyframeRequestsAtLeast { .. } => "CheckKeyframeRequestsAtLeast",
        Step::RecoverVideoAfterLostKeyframe { .. } => "RecoverVideoAfterLostKeyframe",
        Step::CheckKeyframeRecovery { .. } => "CheckKeyframeRecovery",
        Step::CheckAdmissionFailed { .. } => "CheckAdmissionFailed",
        Step::CheckParticipantsKnown { .. } => "CheckParticipantsKnown",
        Step::CheckIdentityStable { .. } => "CheckIdentityStable",
        Step::CheckConnected { .. } => "CheckConnected",
        Step::CheckNotConnected { .. } => "CheckNotConnected",
        Step::CheckRxBytes { .. } => "CheckRxBytes",
        Step::CheckNativeAudioDecoded { .. } => "CheckNativeAudioDecoded",
        Step::WaitForNativeAudioRecovery { .. } => "WaitForNativeAudioRecovery",
        Step::CheckNativeAudioQuiet { .. } => "CheckNativeAudioQuiet",
        Step::CheckNativeAudioSilent { .. } => "CheckNativeAudioSilent",
        Step::CheckEncodedAudio { .. } => "CheckEncodedAudio",
        Step::CheckEncodedAudioInterval { .. } => "CheckEncodedAudioInterval",
        Step::CheckAudioCatalogFrom { .. } => "CheckAudioCatalogFrom",
        Step::CheckAudioSlotStreams { .. } => "CheckAudioSlotStreams",
        Step::CheckTxBytes { .. } => "CheckTxBytes",
        Step::CheckRxBytesInterval { .. } => "CheckRxBytesInterval",
        Step::CheckMaxRxBytesInterval { .. } => "CheckMaxRxBytesInterval",
        Step::SetBandwidth { .. } => "SetBandwidth",
        Step::SetCapacity { .. } => "SetCapacity",
        Step::SetLoss { .. } => "SetLoss",
        Step::SetReorder { .. } => "SetReorder",
        Step::Expect { .. } => "Expect",
        Step::Report { .. } => "Report",
        Step::CheckTxBytesInterval { .. } => "CheckTxBytesInterval",
        Step::CheckDataReceived { .. } => "CheckDataReceived",
        Step::CheckDataNotReceived { .. } => "CheckDataNotReceived",
        Step::CheckDataCount { .. } => "CheckDataCount",
        Step::CheckDataSequence { .. } => "CheckDataSequence",
        Step::CheckTopicIdentity { .. } => "CheckTopicIdentity",
        Step::StopServer => "StopServer",
        Step::RestartServer => "RestartServer",
        Step::WaitForTopicPublisher { .. } => "WaitForTopicPublisher",
        Step::OrderedTopicRecovery { .. } => "OrderedTopicRecovery",
        Step::WaitForDataSequence { .. } => "WaitForDataSequence",
        Step::SetAudioIntent { .. } => "SetAudioIntent",
        Step::CheckSpeakerHeld { .. } => "CheckSpeakerHeld",
    }
}

type PlanHandles = BTreeMap<&'static str, ParticipantHandle>;
type PlanIps = BTreeMap<&'static str, IpAddr>;

fn source_context(
    handles: &PlanHandles,
    participant: &str,
    publisher: &str,
) -> anyhow::Result<(String, u64)> {
    let source = handles
        .get(publisher)
        .context("native publisher unavailable")?;
    let admission = source
        .shared
        .observation
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|observation| observation.admission)
        .context("native publisher admission unavailable")?;
    let origin = super::provenance::current_source_key(
        &source.shared.h264_publishers.lock().unwrap(),
        &admission.to_string(),
        "v0",
    )
    .context("native publisher has no constructed fixture source")?;
    let handle = handles
        .get(participant)
        .context("native viewer unavailable")?;
    let generation = handle
        .shared
        .observation
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|observation| observation.snapshot.generation)
        .context("native viewer generation unavailable")?
        .get();
    Ok((origin, generation))
}

fn source_request(
    handles: &PlanHandles,
    participant: &str,
    publisher: &str,
) -> anyhow::Result<VideoSubscription> {
    let id = handles
        .get(publisher)
        .context("publisher unavailable")?
        .shared
        .participant_id
        .lock()
        .unwrap()
        .clone()
        .context("publisher not admitted")?;
    handles
        .get(participant)
        .context("viewer unavailable")?
        .shared
        .pending_ops
        .lock()
        .unwrap()
        .intent
        .video
        .iter()
        .find(|request| request.participant_id == id)
        .cloned()
        .context("source phase needs an explicit application request")
}

fn current_source_proof(
    handles: &PlanHandles,
    participant: &str,
    publisher: &str,
    since: Instant,
) -> anyhow::Result<super::native_origin::NativeOriginProof> {
    let (origin, generation) = source_context(handles, participant, publisher)?;
    let log = handles
        .get(participant)
        .context("viewer unavailable")?
        .shared
        .video_rx
        .lock()
        .unwrap();
    let expected = handles
        .get(publisher)
        .and_then(|source| source.shared.solid_picture.as_ref())
        .map(|picture| picture.expected());
    let proof = log
        .native_origin
        .require_current_content(&origin, generation, since, expected)?;
    log.native_decoded
        .require_valid_receiver(generation, proof.track.receiver)?;
    Ok(proof)
}

fn native_source_proof(
    handles: &PlanHandles,
    participant: &str,
    publisher: &str,
    min_frames: u64,
    min_resolution: (u32, u32),
) -> anyhow::Result<super::native_origin::NativeOriginProof> {
    let (origin, generation) = source_context(handles, participant, publisher)?;
    let handle = handles
        .get(participant)
        .context("native viewer unavailable")?;
    let log = handle.shared.video_rx.lock().unwrap();
    let proof = log.native_origin.require_since(
        &origin,
        generation,
        handle.native_origin_interval,
        min_frames,
        min_resolution,
    )?;
    // Lossy legacy playback permits decoder concealment. Exact fixture pixels
    // and event deadlines belong to explicit clean-source phases.
    log.native_decoded
        .require_valid_receiver(generation, proof.track.receiver)?;
    let statistics = handle.shared.statistics.lock().unwrap();
    let stats = statistics
        .as_ref()
        .context("native decoder statistics unavailable")?;
    anyhow::ensure!(
        stats.generation == generation,
        "native decoder report is from an old generation"
    );
    let interval = handle.shared.interval_statistics.lock().unwrap();
    if let Some(previous) = interval.sample()
        && previous.generation == generation
    {
        anyhow::ensure!(
            stats.timestamp_us > previous.timestamp_us,
            "native decoder report did not advance"
        );
    }
    super::native_video::require_decoder_report(
        &stats.report_json,
        &proof.track.mid,
        proof.dimensions,
        min_frames,
    )?;
    Ok(proof)
}

enum ServerCommand {
    Stop(tokio::sync::oneshot::Sender<()>),
    Restart,
}

struct ServerControl {
    commands: mpsc::Sender<ServerCommand>,
    startup_notifications: mpsc::Receiver<()>,
}

async fn execute_plan(
    plan: Vec<Step>,
    handles: &mut PlanHandles,
    name_to_ip: &PlanIps,
    reports: &Mutex<HashMap<&'static str, LinkReport>>,
    server: &mut ServerControl,
    link: LinkProfile,
) -> anyhow::Result<()> {
    let total = plan.len();
    // Duration of the most recent Run, so interval properties know what window they describe.
    let mut window = Duration::ZERO;
    let mut controller_stall_until = None;
    let mut disturbances = BTreeSet::new();
    let mut changed_paths = BTreeSet::new();

    for (idx, step) in plan.iter().enumerate() {
        let n = idx + 1;
        let kind = step_name(step);
        match step {
            Step::SetCapacity { participant, .. }
            | Step::SetBandwidth { participant, .. }
            | Step::SetLoss { participant, .. }
            | Step::SetReorder { participant, .. } => {
                changed_paths.insert(*participant);
            }
            _ => {}
        }

        match step {
            Step::IntentHistory { history, witness } => {
                histories::execute(history, witness, handles, name_to_ip).await?;
            }
            Step::StopServer => {
                let (stopped, complete) = tokio::sync::oneshot::channel();
                server.commands.send(ServerCommand::Stop(stopped)).await?;
                complete.await?;
                disturbances.insert((2, "server", "server"));
            }
            Step::RestartServer => {
                server.commands.send(ServerCommand::Restart).await?;
                server.startup_notifications.recv().await.ok_or_else(|| {
                    anyhow::anyhow!("restarted server did not supply startup acknowledgment")
                })?;
                disturbances.remove(&(2, "server", "server"));
            }
            Step::WaitForTopicPublisher {
                participant,
                topic,
                mode,
                next_sequence,
            } => {
                let handle = get_handle(handles, participant, kind)?;
                tokio::time::timeout(Duration::from_secs(30), async {
                    loop {
                        let ready = handle
                            .shared
                            .observation
                            .lock()
                            .unwrap()
                            .as_ref()
                            .is_some_and(|observation| {
                                observation
                                    .snapshot
                                    .topics
                                    .publishers
                                    .iter()
                                    .any(|publisher| {
                                        publisher.registration.topic == *topic
                                            && publisher.registration.mode == *mode
                                            && publisher.data_track_id.is_some()
                                            && !publisher.send_pending
                                            && publisher.queued_messages == 0
                                            && publisher.next_sequence == *next_sequence
                                    })
                            });
                        if ready {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .map_err(|_| {
                    anyhow::anyhow!("{participant} did not transmit topic sequence {next_sequence}")
                })?;
            }
            Step::OrderedTopicRecovery {
                publisher,
                topic,
                recovering,
                late,
            } => {
                ordered_recovery::execute(handles, name_to_ip, publisher, topic, *recovering, late)
                    .await?;
                changed_paths.extend(recovering);
            }
            Step::WaitForDataSequence {
                participant,
                topic,
                expected,
            } => {
                let handle = get_handle(handles, participant, kind)?;
                let expected: Vec<Vec<u8>> =
                    expected.iter().map(|payload| payload.to_vec()).collect();
                tokio::time::timeout(Duration::from_secs(30), async {
                    loop {
                        let actual = handle.shared.topic_events.lock().unwrap().get(*topic).map(|events| events.iter().filter_map(topic_payload).map(<[u8]>::to_vec).collect::<Vec<_>>()).unwrap_or_default();
                        if !expected.starts_with(&actual) {
                            let mismatch = actual.iter().zip(expected.iter()).position(|(actual, expected)| actual != expected).unwrap_or(expected.len());
                            let events = handle.shared.topic_events.lock().unwrap();
                            let events = events.get(*topic).map(Vec::as_slice).unwrap_or_default();
                            let sequences = events.iter().filter_map(|event| match event { TopicNotification::Message(message) => Some(message.sequence), _ => None }).take(40).collect::<Vec<_>>();
                            let gaps = events.iter().filter(|event| matches!(event, TopicNotification::RecoveryGap { .. })).collect::<Vec<_>>();
                            anyhow::bail!("{participant} received unexpected sequence at index {mismatch}: actual_len={} expected_len={} actual={:?} expected={:?} first_sequences={sequences:?} gaps={gaps:?}", actual.len(), expected.len(), actual.get(mismatch), expected.get(mismatch));
                        }
                        if actual == expected { return Ok::<_, anyhow::Error>(()); }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await.map_err(|_| {
                    let events = handle.shared.topic_events.lock().unwrap();
                    let events = events.get(*topic).map(Vec::as_slice).unwrap_or_default();
                    let actual_len = events.iter().filter_map(topic_payload).count();
                    let sequences = events.iter().filter_map(|event| match event {
                        TopicNotification::Message(message) => Some(message.sequence),
                        _ => None,
                    }).collect::<Vec<_>>();
                    let first = &sequences[..sequences.len().min(40)];
                    let last = &sequences[sequences.len().saturating_sub(40)..];
                    let gaps = events.iter().filter(|event| matches!(event, TopicNotification::RecoveryGap { .. })).collect::<Vec<_>>();
                    let observation = handle.shared.observation.lock().unwrap();
                    let generation = observation.as_ref().and_then(|state| state.snapshot.generation).map(pulsebeam_agent_native::agent_core::Generation::get);
                    let connection = observation.as_ref().map(|state| &state.snapshot.connection);
                    let subscribers = observation.as_ref().map(|state| state.snapshot.topics.subscribers.iter().filter(|status| status.registration.topic == *topic).take(16).collect::<Vec<_>>());
                    anyhow::anyhow!("{participant} did not receive ordered sequence on {topic}: actual_len={actual_len} expected_len={} first_sequences={first:?} last_sequences={last:?} gaps={gaps:?} generation={generation:?} connection={connection:?} subscribers={subscribers:?}", expected.len())
                })??;
            }
            Step::Run {
                description,
                duration,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({duration:?})");
                for handle in handles.values_mut() {
                    handle.snapshot_interval();
                }
                // Same windowing as the byte baselines, so the checks describe this step.
                pulsebeam::sim_metrics::reset();
                pulsebeam_runtime::net::shaper::reset_stats_for(name_to_ip.values().copied());
                window = *duration;
                tokio::time::sleep(*duration).await;
                // Every plan, every run step, every participant. A settled room is the only place
                // this is fair to ask - discovery and teardown both need a moment - and a
                // `Step::Run` is exactly that moment. Short runs are skipped: under a second is
                // not settling, it is a pause mid-transition.
                if *duration >= ROOM_SETTLE_FLOOR {
                    assert_room_state_consistent(handles, description);
                    if disturbances.is_empty() {
                        assert_convergence(handles, description).await?;
                    }
                }
            }

            Step::WaitForPublication {
                description,
                publisher,
                timeout,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({publisher}, {timeout:?})"
                );
                let subject = handles.get(publisher).ok_or_else(|| {
                    anyhow::anyhow!("step \"{description}\": unknown publisher {publisher}")
                })?;
                anyhow::ensure!(
                    subject.present && subject.publishes_video,
                    "step \"{description}\": {publisher} is not a present video publisher"
                );
                let observers: Vec<_> = handles
                    .iter()
                    .filter(|(name, handle)| {
                        **name != *publisher
                            && handle.present
                            && handle.observes_video
                            && handle.room_name == subject.room_name
                    })
                    .collect();
                anyhow::ensure!(
                    !observers.is_empty(),
                    "step \"{description}\": no video observers for {publisher}"
                );
                let mut pending_convergence = None;
                tokio::time::timeout(*timeout, async {
                    loop {
                        if subject.connected()
                            && let Some(id) = subject.participant_id()
                            && observers.iter().all(|(_, observer)| {
                                observer.connected()
                                    && observer.media_kinds_of(&id)
                                        == Some((subject.publishes_video, subject.publishes_audio))
                            })
                            // The room oracle needs every present client's admission
                            // and first coherent snapshot, not only this publication.
                            && handles.values().all(|handle| {
                                !handle.present
                                    || (handle.participant_id().is_some()
                                        && handle.shared.observation.lock().unwrap().is_some())
                            })
                        {
                            assert_room_state_consistent(handles, description);
                            if !disturbances.is_empty() {
                                break;
                            }
                            // Receiver discovery is not an all-client barrier: publish-only
                            // catalogs and other clients' public assignments progress independently.
                            match assert_convergence(handles, description).await {
                                Ok(()) => break,
                                Err(error) => pending_convergence = Some(error),
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .map_err(|_| anyhow::anyhow!("step \"{description}\": {publisher} publication did not converge within {timeout:?}; last convergence failure: {pending_convergence:?}"))?;
            }

            Step::WaitForVideo {
                description,
                participant,
                publishers,
                timeout,
                ..
            }
            | Step::WaitForVideoAtResolution {
                description,
                participant,
                publishers,
                timeout,
                ..
            } => {
                let min_resolution = match step {
                    Step::WaitForVideo { .. } => None,
                    Step::WaitForVideoAtResolution { min_resolution, .. } => Some(*min_resolution),
                    _ => unreachable!(),
                };
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, {publishers:?}, {timeout:?})"
                );
                anyhow::ensure!(
                    !publishers.is_empty(),
                    "{description}: no video origins requested"
                );
                let subscriber = get_handle(handles, participant, description)?;
                let received = subscriber.video_rx();
                let origins = publishers
                    .iter()
                    .map(|publisher| {
                        let id = get_handle(handles, publisher, description)?
                            .participant_id()
                            .ok_or_else(|| {
                                anyhow::anyhow!("{description}: {publisher} is not admitted")
                            })?;
                        let before = received
                            .decoded_from(&id)
                            .map_or(0, |decoded| decoded.frames);
                        Ok((*publisher, id, before))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                for handle in handles.values_mut() {
                    handle.snapshot_interval();
                }
                pulsebeam::sim_metrics::reset();
                pulsebeam_runtime::net::shaper::reset_stats_for(name_to_ip.values().copied());
                let started = std::time::Instant::now();
                let mut last = Vec::new();
                let mut pending_convergence = None;
                tokio::time::timeout(*timeout, async {
                    loop {
                        last = {
                            let log = get_handle(handles, participant, description)?.shared.video_rx.lock().unwrap();
                            origins.iter().map(|(publisher, id, before)| {
                                let decoded = log.decoded_from(id);
                                let frames = decoded.map_or(0, |decoded| decoded.frames)
                                    .saturating_sub(*before);
                                let resolution = decoded.map(|decoded| (decoded.width, decoded.height));
                                (*publisher, frames, resolution)
                            }).collect()
                        };
                        if last.iter().all(|(_, frames, resolution)| {
                            *frames > 0
                                && min_resolution.is_none_or(|minimum| resolution.is_some_and(|actual| actual.0 >= minimum.0 && actual.1 >= minimum.1))
                        }) {
                            match assert_convergence(handles, description).await {
                                Ok(()) => break Ok::<_, anyhow::Error>(()),
                                Err(error) => pending_convergence = Some(error),
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }).await.map_err(|_| anyhow::anyhow!(
                    "{description}: video did not converge within {timeout:?}; recipient={participant}, origins={last:?}, last convergence failure={pending_convergence:?}"
                ))??;
                window = started.elapsed();
            }

            Step::BeginControllerStall { duration } => {
                tracing::info!("[step {n}/{total}: {kind}] controller stalled for {duration:?}");
                anyhow::ensure!(
                    controller_stall_until.is_none(),
                    "controller hold already pending"
                );
                let (until, begun) = pulsebeam::sim_metrics::request_controller_stall(*duration);
                tokio::time::timeout(*duration, begun).await??;
                anyhow::ensure!(
                    Instant::now() < until,
                    "controller did not enter the declared hold"
                );
                controller_stall_until = Some(until);
            }

            Step::WaitForControllerStallEnd => {
                let until = controller_stall_until
                    .take()
                    .context("no controller hold pending")?;
                anyhow::ensure!(
                    Instant::now() < until,
                    "media checkpoint did not precede hold end"
                );
                tokio::time::sleep(until.saturating_duration_since(Instant::now())).await;
            }

            Step::ReconnectThroughForeignShard {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                // Count completed application builds; readmission may preserve the member ID.
                let previous = get_handle(handles, participant, description)?
                    .shared
                    .incarnations
                    .lock()
                    .unwrap()
                    .len();
                let source = resolve(name_to_ip, participant, description)?;
                let injected = pulsebeam_runtime::net::set_wrong_owner_injection(source);
                handles
                    .get(participant)
                    .expect("participant was resolved above")
                    .send_command(ParticipantCmd::Reconnect);
                tokio::time::timeout(Duration::from_secs(5), async {
                    injected
                        .await
                        .context("foreign-shard bootstrap input cancelled")?;
                    loop {
                        let handle = get_handle(handles, participant, description)?;
                        if handle.connected()
                            && handle.shared.incarnations.lock().unwrap().len() > previous
                        {
                            break Ok::<_, anyhow::Error>(());
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .context(
                    "foreign-shard bootstrap did not settle within the initial admission horizon",
                )??;
            }

            Step::FailNextMaterialization { description } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\"");
                pulsebeam::sim_metrics::fail_next_materialization();
            }

            Step::Report {
                description,
                participant,
            } => {
                let ip = resolve(name_to_ip, participant, description)?;
                let handle = handles.get(participant).ok_or_else(|| {
                    anyhow::anyhow!("step \"{description}\": unknown participant {participant}")
                })?;
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}) {}",
                    report_metrics(handle, ip, window)
                );
            }

            Step::SetCapacity {
                description,
                participant,
                capacity,
                max_backlog,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, {capacity:?})"
                );
                let ip = resolve(name_to_ip, participant, description)?;
                pulsebeam_runtime::net::shaper::set_capacity(ip, *capacity, *max_backlog);
            }

            Step::SetReorder {
                description,
                participant,
                reorder,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, {reorder:?})"
                );
                let ip = resolve(name_to_ip, participant, description)?;
                pulsebeam_runtime::net::shaper::set_reorder(ip, *reorder);
            }

            Step::SetLoss {
                description,
                participant,
                loss,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, {loss:?})"
                );
                let ip = resolve(name_to_ip, participant, description)?;
                pulsebeam_runtime::net::shaper::set_loss(ip, *loss);
            }

            Step::Expect {
                description,
                participant,
                property,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, {property:?})"
                );
                let ip = resolve(name_to_ip, participant, description)?;
                let handle = handles.get(participant).ok_or_else(|| {
                    anyhow::anyhow!("step \"{description}\": unknown participant {participant}")
                })?;
                if let Err(reason) = check_property(property, handle, ip) {
                    // Print the full picture alongside the failure: the one number that broke
                    // rarely explains why on its own.
                    tracing::error!("  context: {}", report_metrics(handle, ip, window));
                    panic!(
                        "\nproperty not satisfied\n  plan step:   {n}/{total} {kind}\n  \
                         description: \"{description}\"\n  participant:  {participant}\n  \
                         property:     {property:?}\n  actual:       {reason}"
                    );
                }
            }

            Step::Partition {
                description,
                from,
                to,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({from} ↔ {to})");
                let from_ip = resolve(name_to_ip, from, description)?;
                let to_ip = resolve(name_to_ip, to, description)?;
                disturbances.insert((0, (*from).min(*to), (*from).max(*to)));
                turmoil::partition(from_ip, to_ip);
            }

            Step::Repair {
                description,
                from,
                to,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({from} ↔ {to})");
                let from_ip = resolve(name_to_ip, from, description)?;
                let to_ip = resolve(name_to_ip, to, description)?;
                disturbances.remove(&(0, (*from).min(*to), (*from).max(*to)));
                turmoil::repair(from_ip, to_ip);
            }

            Step::Hold {
                description,
                from,
                to,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({from} ↔ {to})");
                let from_ip = resolve(name_to_ip, from, description)?;
                let to_ip = resolve(name_to_ip, to, description)?;
                disturbances.insert((1, (*from).min(*to), (*from).max(*to)));
                turmoil::hold(from_ip, to_ip);
            }

            Step::Release {
                description,
                from,
                to,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({from} ↔ {to})");
                let from_ip = resolve(name_to_ip, from, description)?;
                let to_ip = resolve(name_to_ip, to, description)?;
                disturbances.remove(&(1, (*from).min(*to), (*from).max(*to)));
                turmoil::release(from_ip, to_ip);
            }

            Step::Join {
                description,
                participant,
            }
            | Step::JoinOnce {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                *handle.shared.driver_admission_retry.lock().unwrap() =
                    matches!(step, Step::JoinOnce { .. });
                if !handle.shared.incarnations.lock().unwrap().is_empty() {
                    handle.shared.pending_ops.lock().unwrap().restart();
                }
                handle.present = true;
                handle.send_command(ParticipantCmd::Reconnect);
            }

            Step::Disconnect {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                // Ground truth for the room invariant, from the plan rather than the wire.
                handle.present = false;
                handle.retirement_deadline = None;
                handle.shared.pending_ops.lock().unwrap().restart();
                handle.send_command(ParticipantCmd::Shutdown);
            }

            Step::AbruptExit {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                // Ground truth for the room invariant, from the plan rather than the wire.
                handle.present = false;
                handle.retirement_deadline = Some(Instant::now() + CRASH_RETIREMENT_BUDGET);
                handle.send_command(ParticipantCmd::Drop);
            }

            Step::Reconnect {
                description,
                participant,
            }
            | Step::BeginHeldReconnect {
                description,
                participant,
                ..
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                if matches!(step, Step::BeginHeldReconnect { .. }) {
                    handle.shared.bootstrap_hold.arm()?;
                }
                // This command rebuilds the harness driver, unlike native ICE reconnection.
                handle.shared.pending_ops.lock().unwrap().restart();
                handle.present = true;
                handle.send_command(ParticipantCmd::Reconnect);
                if let Step::BeginHeldReconnect { timeout, .. } = step {
                    let deadline = Instant::now() + *timeout;
                    while handle.shared.bootstrap_hold.started().is_none() {
                        anyhow::ensure!(
                            Instant::now() < deadline,
                            "replacement bootstrap never entered hold"
                        );
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    anyhow::ensure!(
                        handle.shared.bootstrap_hold.pending(),
                        "replacement bootstrap was cancelled"
                    );
                }
            }

            Step::ReleaseHeldReconnect {
                description,
                participant,
                held_for,
            } => {
                let handle = get_handle(handles, participant, description)?;
                let hold = &handle.shared.bootstrap_hold;
                let started = hold
                    .started()
                    .context("replacement bootstrap never entered hold")?;
                let deadline = started
                    .checked_add(*held_for)
                    .context("bootstrap hold overflow")?;
                anyhow::ensure!(
                    Instant::now() <= deadline,
                    "source proof exceeded bootstrap hold"
                );
                tokio::time::sleep(deadline.saturating_duration_since(Instant::now())).await;
                anyhow::ensure!(
                    Instant::now() == deadline,
                    "bootstrap release missed declared hold deadline"
                );
                hold.release()?;
                tracing::info!(
                    ?started,
                    ?held_for,
                    "released pending replacement bootstrap after source proof"
                );
            }

            Step::WaitSinceBootstrapRelease {
                description,
                participant,
                duration,
            } => {
                let handle = get_handle(handles, participant, description)?;
                let released = handle
                    .shared
                    .bootstrap_hold
                    .released()
                    .context("bootstrap hold was not released")?;
                let deadline = released
                    .checked_add(*duration)
                    .context("recovery budget overflow")?;
                anyhow::ensure!(
                    Instant::now() <= deadline,
                    "native service missed restoration budget"
                );
                tokio::time::sleep(deadline.saturating_duration_since(Instant::now())).await;
                tracing::info!(
                    ?released,
                    ?deadline,
                    "declared bootstrap recovery budget ended"
                );
            }

            Step::SubscribeToConfigured {
                description,
                participant,
                targets,
            } => {
                let subscriptions = targets
                    .iter()
                    .map(|request| {
                        let publisher = handles
                            .get(request.participant_id.as_str())
                            .context("configured publisher unavailable")?;
                        let id = publisher
                            .shared
                            .participant_id
                            .lock()
                            .unwrap()
                            .clone()
                            .context("configured publisher not admitted")?;
                        Ok(VideoSubscription {
                            participant_id: id,
                            ..request.clone()
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let handle = get_handle(handles, participant, description)?;
                handle.subscribed_at.get_or_insert_with(Instant::now);
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::SetSubscriptions(subscriptions));
            }
            Step::SetSubscriptions {
                description,
                participant,
                subscriptions,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                // Time-to-first-frame runs from the moment the viewer asked, not from plan start.
                handle.subscribed_at.get_or_insert_with(Instant::now);
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::SetSubscriptions(subscriptions.clone()));
            }

            Step::RelabelOwnedVideo {
                description,
                participant,
                label,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, label={label})"
                );
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::RelabelOwnedVideo((*label).into()));
            }

            Step::SubscribeTo {
                description,
                participant,
                targets,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, targets={targets:?})"
                );
                let mut subs: Vec<VideoSubscription> = Vec::new();
                for (name, height) in *targets {
                    let pub_handle = get_handle(handles, name, description)?;
                    let id = pub_handle.shared.participant_id.lock().unwrap().clone();
                    let id = id.ok_or_else(|| {
                        anyhow::anyhow!(
                            "step \"{description}\": SubscribeTo target \"{name}\" has no runtime participant id yet; add a Step::Run before this step"
                        )
                    })?;
                    subs.push(VideoSubscription {
                        participant_id: id,
                        publication: None,
                        height: *height,
                        ..Default::default()
                    });
                }
                let handle = get_handle(handles, participant, description)?;
                // Time-to-first-frame runs from the moment the viewer asked, not from plan start.
                handle.subscribed_at.get_or_insert_with(Instant::now);
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::SetSubscriptions(subs));
            }

            Step::SubscribeToQos {
                description,
                participant,
                targets,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, targets={targets:?})"
                );
                let mut subs: Vec<VideoSubscription> = Vec::new();
                for (name, height, min_height, priority) in *targets {
                    let pub_handle = get_handle(handles, name, description)?;
                    let id = pub_handle.shared.participant_id.lock().unwrap().clone();
                    let id = id.ok_or_else(|| {
                        anyhow::anyhow!(
                            "step \"{description}\": SubscribeToQos target \"{name}\" has no runtime participant id yet; add a Step::Run before this step"
                        )
                    })?;
                    subs.push(VideoSubscription {
                        participant_id: id,
                        publication: None,
                        height: *height,
                        min_height: *min_height,
                        priority: *priority,
                        ..Default::default()
                    });
                }
                let handle = get_handle(handles, participant, description)?;
                // Time-to-first-frame runs from the moment the viewer asked, not from plan start.
                handle.subscribed_at.get_or_insert_with(Instant::now);
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::SetSubscriptions(subs));
            }

            Step::SetAudioIntent {
                description,
                participant,
                pinned,
                auto,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, pinned={pinned:?}, auto={auto})"
                );
                let mut publishers = Vec::new();
                for name in *pinned {
                    let pub_handle = get_handle(handles, name, description)?;
                    let id = pub_handle.shared.participant_id.lock().unwrap().clone();
                    let id = id.ok_or_else(|| {
                        anyhow::anyhow!(
                            "step \"{description}\": pin target \"{name}\" has no runtime participant id yet; add a Step::Run before this step"
                        )
                    })?;
                    publishers.push(id);
                }
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::SetAudioIntent(publishers, *auto));
            }

            Step::SubscribeAll {
                description,
                participant,
                heights,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, heights={heights:?})"
                );
                let handle = get_handle(handles, participant, description)?;
                // Time-to-first-frame runs from the moment the viewer asked, not from plan start.
                handle.subscribed_at.get_or_insert_with(Instant::now);
                let mut tracks: Vec<String> = handle
                    .shared
                    .discovered_tracks
                    .lock()
                    .unwrap()
                    .iter()
                    .cloned()
                    .collect();
                tracks.sort();
                if tracks.is_empty() {
                    anyhow::bail!(
                        "step \"{description}\": SubscribeAll on \"{participant}\" but no tracks discovered yet; add a Step::Run before this step"
                    );
                }
                let heights_slice = if heights.is_empty() {
                    &[720u32][..]
                } else {
                    *heights
                };
                let subs: Vec<VideoSubscription> = tracks
                    .iter()
                    .enumerate()
                    .map(|(i, participant_id)| VideoSubscription {
                        participant_id: participant_id.clone(),
                        publication: None,
                        height: heights_slice[i % heights_slice.len()],
                        ..Default::default()
                    })
                    .collect();
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::SetSubscriptions(subs));
            }

            Step::DeclarePublishTopic {
                description,
                participant,
                topic,
                mode,
            } => {
                let handle = get_handle(handles, participant, description)?;
                handle.shared.pending_ops.lock().unwrap().push(
                    PendingDriverOp::DeclarePublishTopic(topic.to_string(), *mode),
                );
            }
            Step::DeclareSubscribeTopic {
                description,
                participant,
                topic,
                mode,
            } => {
                let handle = get_handle(handles, participant, description)?;
                handle.shared.pending_ops.lock().unwrap().push(
                    PendingDriverOp::DeclareSubscribeTopic(topic.to_string(), *mode),
                );
            }
            Step::PublishData {
                description,
                participant,
                topic,
                mode,
                data,
            } => {
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::PublishData(
                        topic.to_string(),
                        *mode,
                        data.to_vec(),
                    ));
            }

            Step::CheckNativeDecodedVideo {
                description,
                participant,
                dimensions,
                min_fresh_frames,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                let generation = handle
                    .shared
                    .observation
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|observation| observation.snapshot.generation)
                    .context("native decoded proof requires a live generation")?
                    .get();
                let mut log = handle.shared.video_rx.lock().unwrap();
                let track = log
                    .native_decoded
                    .require(
                        &handle.native_video_baseline,
                        generation,
                        *dimensions,
                        *min_fresh_frames,
                        Instant::now(),
                    )
                    .with_context(|| format!("{description}: {:?}", log.native_decoded))?;
                let statistics = handle.shared.statistics.lock().unwrap();
                let stats = statistics
                    .as_ref()
                    .context("native decoder statistics unavailable")?;
                anyhow::ensure!(
                    stats.generation == generation,
                    "native decoder statistics are from an old generation"
                );
                anyhow::ensure!(
                    handle
                        .native_video_stats_timestamp
                        .is_none_or(|previous| stats.timestamp_us > previous),
                    "native decoder report did not advance"
                );
                let decoded = super::native_video::require_decoder_report(
                    &stats.report_json,
                    &track.mid,
                    *dimensions,
                    *min_fresh_frames,
                )?;
                tracing::info!(
                    ?track,
                    ?dimensions,
                    min_fresh_frames,
                    "fresh native decoded rung verified; native decoder-error counter is unavailable"
                );
                if let Some((previous_generation, previous_mid, previous_frames)) =
                    &handle.native_video_decoder_frames
                    && *previous_generation == generation
                    && previous_mid == &track.mid
                {
                    let fresh = decoded
                        .checked_sub(*previous_frames)
                        .context("native SDK decoder count reset")?;
                    // RTCStats is sampled asynchronously from pixel delivery.
                    // Require actual SDK progress, not an identical interval count.
                    anyhow::ensure!(fresh > 0, "native SDK decoder count did not advance");
                }
                handle.native_video_decoder_frames = Some((generation, track.mid.clone(), decoded));
                handle.native_video_stats_timestamp = Some(stats.timestamp_us);
                handle.native_video_baseline = log.native_decoded.checkpoint();
            }

            Step::UpdateSolidVideo {
                description,
                participant,
            }
            | Step::QuietSolidVideo {
                description,
                participant,
            } => {
                let handle = get_handle(handles, participant, description)?;
                let input = handle
                    .shared
                    .solid_picture
                    .as_ref()
                    .context("source has no solid-picture input")?;
                if matches!(step, Step::QuietSolidVideo { .. }) {
                    input.pause();
                } else {
                    input.update();
                }
            }
            Step::WaitForSourceVideo {
                description,
                participant,
                publisher,
                min_resolution,
                timeout,
            } => {
                let start = Instant::now();
                get_handle(handles, participant, description)?.native_origin_interval = start;
                let request = source_request(handles, participant, publisher)?;
                let buffer = match request.playout_delay {
                    pulsebeam_agent_native::agent_core::PlayoutDelay::Adaptive => Duration::ZERO,
                    fixed => super::native_origin::SourceTiming {
                        period: Duration::from_secs(1),
                        playout: fixed,
                        fiber_message_latency: Duration::ZERO,
                    }
                    .fixed_buffer()?,
                };
                let mut pending = None;
                tokio::time::timeout(*timeout + buffer, async {
                    loop {
                        match current_source_proof(handles, participant, publisher, start).and_then(|proof| {
                            anyhow::ensure!(proof.dimensions.0 >= min_resolution.0 && proof.dimensions.1 >= min_resolution.1, "source representation not visible yet: {:?}, required {min_resolution:?}", proof.dimensions);
                            Ok(proof)
                        }) {
                            Ok(proof) => {
                                tracing::info!(?proof, ?start, "new supplied capture reached actual native I420");
                                break;
                            }
                            Err(error) => pending = Some(error),
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }).await.with_context(|| format!("{description}: no genuine new source output within {timeout:?} plus {buffer:?}: {pending:?}"))?;
            }
            Step::MeasureSourceWindow {
                description,
                participant,
                publisher,
                encodings,
                duration,
            } => {
                anyhow::ensure!(
                    link.loss == 0.0
                        && link.loss_model.is_none()
                        && link.duplicate == 0.0
                        && link.reorder.probability == 0.0
                        && link.bandwidth_bps.is_none()
                        && link.feedback.is_none(),
                    "strict source window requires clean unshaped fiber"
                );
                anyhow::ensure!(
                    !changed_paths.contains(participant) && !changed_paths.contains(publisher),
                    "strict path was reconfigured"
                );
                anyhow::ensure!(
                    !disturbances.iter().any(|(kind, a, b)| *kind == 2
                        || a == participant
                        || a == publisher
                        || b == participant
                        || b == publisher),
                    "strict source path is held or unavailable"
                );
                let (origin, generation) = source_context(handles, participant, publisher)?;
                let request = source_request(handles, participant, publisher)?;
                anyhow::ensure!(request.height > 0, "strict source was intentionally hidden");
                let handle = handles.get(participant).context("viewer unavailable")?;
                let proof = current_source_proof(
                    handles,
                    participant,
                    publisher,
                    handle.native_origin_interval,
                )?;
                let ledger = handle
                    .shared
                    .h264_publishers
                    .lock()
                    .unwrap()
                    .get(&origin)
                    .and_then(|source| source.ledger.clone())
                    .context("actual source ledger unavailable")?;
                let period = ledger.lock().unwrap().period;
                let start = Instant::now();
                let phase = super::native_origin::SourceWindow {
                    source: origin,
                    track: proof.track,
                    encodings: encodings
                        .iter()
                        .map(|encoding| (*encoding).to_owned())
                        .collect(),
                    start,
                    end: start
                        .checked_add(*duration)
                        .context("source interval overflow")?,
                    timing: super::native_origin::SourceTiming {
                        period,
                        playout: request.playout_delay,
                        fiber_message_latency: link.max_latency,
                    },
                };
                handle
                    .shared
                    .video_rx
                    .lock()
                    .unwrap()
                    .native_origin
                    .begin_source_window(phase.clone())?;
                tokio::time::sleep(*duration).await;
                loop {
                    let drain = phase.drain_until(&ledger.lock().unwrap())?.max(phase.end);
                    let now = Instant::now();
                    if now >= drain {
                        break;
                    }
                    tokio::time::sleep(drain.duration_since(now)).await;
                }
                let mut log = handle.shared.video_rx.lock().unwrap();
                log.native_decoded
                    .require_valid_receiver(generation, phase.track.receiver)?;
                let proof = log
                    .native_origin
                    .finish_source_window(&ledger.lock().unwrap(), Instant::now())
                    .with_context(|| {
                        format!("{description}: complete supplied/native source interval")
                    })?;
                tracing::info!(?proof, ?phase, deadline = ?phase.timing.deadline()?, "complete source interval verified");
            }
            Step::BeginNativePlayback {
                description,
                participant,
                after_pause,
            } => {
                let handle = get_handle(handles, participant, description)?;
                handle.native_playback_started = true;
                handle.native_origin_interval = Instant::now();
                let mut log = handle.shared.video_rx.lock().unwrap();
                handle.native_video_baseline = if *after_pause {
                    log.native_decoded.checkpoint_after_pause()
                } else {
                    log.native_decoded.checkpoint()
                };
            }
            Step::WaitForNativeVideo {
                description,
                participant,
                publisher,
                min_resolution,
                timeout,
            } => {
                get_handle(handles, participant, description)?.native_origin_interval =
                    Instant::now();
                let mut pending = None;
                tokio::time::timeout(*timeout, async {
                    loop {
                        match native_source_proof(handles, participant, publisher, 2, *min_resolution)
                            .and_then(|proof| {
                                let handle = handles.get(participant).context("viewer unavailable")?;
                                let log = handle.shared.video_rx.lock().unwrap();
                                // First visible output is distinct from the subsequent
                                // continuous-playback contract.
                                log.native_decoded.require_recent(&proof.track, Instant::now())?;
                                Ok(proof)
                            }) {
                            Ok(_) => match assert_convergence(handles, description).await {
                                Ok(()) => break,
                                Err(error) => pending = Some(error),
                            },
                            Err(error) => pending = Some(error),
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    anyhow::Ok(())
                }).await.with_context(|| format!("{description}: first native output did not arrive within {timeout:?}; last={pending:?}"))??;
            }
            Step::CheckNativePlayback {
                description,
                participant,
                publisher,
                expected,
                same_receiver,
            } => {
                let proof = native_source_proof(
                    handles,
                    participant,
                    publisher,
                    2,
                    expected.min_resolution,
                )
                .inspect_err(|_| {
                    if let Some(handle) = handles.get(participant)
                        && let Some(stats) = handle.shared.statistics.lock().unwrap().as_ref()
                    {
                        tracing::error!(
                            participant,
                            publisher,
                            generation = stats.generation,
                            timestamp_us = stats.timestamp_us,
                            report = %String::from_utf8_lossy(
                                &stats.report_json[..stats.report_json.len().min(65_536)]
                            ),
                            "native decoder statistics at source-proof failure"
                        );
                    }
                })?;
                let handle = get_handle(handles, participant, description)?;
                anyhow::ensure!(
                    handle.native_playback_started,
                    "native playback window was not started"
                );
                if *same_receiver {
                    let previous = handle
                        .native_playback_track
                        .as_ref()
                        .context("no previous slot playback")?;
                    anyhow::ensure!(
                        previous.generation == proof.track.generation
                            && previous.receiver == proof.track.receiver
                            && previous.track_id == proof.track.track_id
                            && previous.mid == proof.track.mid
                            && previous.media_index == proof.track.media_index,
                        "replacement did not reuse the original native receiver"
                    );
                }
                handle
                    .shared
                    .video_rx
                    .lock()
                    .unwrap()
                    .native_decoded
                    .require_playback(
                        &handle.native_video_baseline,
                        &proof.track,
                        *expected,
                        Instant::now(),
                    )?;
                handle.native_playback_track = Some(proof.track.clone());
                tracing::info!(description, participant, publisher, ?expected, ?proof.track, "source-authenticated native playback verified");
            }
            Step::CheckNativeVideoDecodedFromInterval {
                description,
                participant,
                publisher,
                min_frames,
                min_resolution,
            } => {
                let proof = native_source_proof(
                    handles,
                    participant,
                    publisher,
                    *min_frames,
                    *min_resolution,
                )
                .with_context(|| format!("{description}: {participant} <- {publisher}"))?;
                tracing::info!(description, participant, publisher, ?proof.track,
                    ?proof.dimensions, native_frames = proof.frames,
                    "authenticated admission-specific native I420 progress verified");
            }

            Step::CheckNativeSourceProgress {
                description,
                participant,
                publisher,
                min_frames,
                min_resolution,
            } => {
                let (source, generation) = source_context(handles, participant, publisher)?;
                let handle = handles
                    .get(participant)
                    .with_context(|| format!("{description}: participant unavailable"))?;
                let log = handle.shared.video_rx.lock().unwrap();
                log.native_decoded.require_callback_collector()?;
                let proof = log
                    .native_origin
                    .require_progress(
                        &source,
                        generation,
                        handle.native_origin_interval,
                        *min_frames,
                        *min_resolution,
                    )
                    .inspect_err(|_| {
                        tracing::error!(
                            participant,
                            publisher,
                            decoded = ?log.native_decoded,
                            "native callback facts at source-progress failure"
                        );
                        for owner in [participant, publisher] {
                            if let Some(peer) = handles.get(owner)
                                && let Some(stats) = peer.shared.statistics.lock().unwrap().as_ref()
                            {
                                tracing::error!(
                                    owner,
                                    participant,
                                    publisher,
                                    generation = stats.generation,
                                    timestamp_us = stats.timestamp_us,
                                    report = %String::from_utf8_lossy(
                                        &stats.report_json[..stats.report_json.len().min(65_536)]
                                    ),
                                    "native peer statistics at source-progress failure"
                                );
                            }
                        }
                    })
                    .with_context(|| format!("{description}: {participant} <- {publisher}"))?;
                log.native_decoded
                    .require_valid_receiver(generation, proof.track.receiver)?;
                tracing::info!(
                    description,
                    participant,
                    publisher,
                    ?proof.track,
                    ?proof.min_dimensions,
                    native_frames = proof.frames,
                    "phase-wide source-correlated native I420 progress verified"
                );
            }
            Step::CheckSourceInputLoad {
                description,
                participant,
                protected,
                competing,
                competing_tier,
                capacity_bps,
                max_backlog,
            } => {
                let handle = handles
                    .get(participant)
                    .context("input load viewer unavailable")?;
                let start = handle.native_origin_interval;
                let end = Instant::now();
                let elapsed = end.duration_since(start).as_secs_f64();
                let load = |publisher, encoding, max_temporal| -> anyhow::Result<_> {
                    let (source, _) = source_context(handles, participant, publisher)?;
                    let ledger = handle
                        .shared
                        .h264_publishers
                        .lock()
                        .unwrap()
                        .get(&source)
                        .and_then(|source| source.ledger.clone())
                        .context("source input ledger unavailable")?;
                    let ledger = ledger.lock().unwrap();
                    let input = ledger.input_load(encoding, max_temporal, start, end)?;
                    let period = ledger
                        .period
                        .checked_mul(if max_temporal == Some(1) { 2 } else { 1 })
                        .context("input cadence period overflow")?;
                    anyhow::ensure!(!period.is_zero(), "input cadence period is zero");
                    let required = end.duration_since(start).as_nanos() / period.as_nanos();
                    anyhow::ensure!(
                        input.offers.len() as u128 >= required,
                        "{description}: {publisher}/{encoding} supplied {} captures, needs {required} for declared cadence",
                        input.offers.len()
                    );
                    Ok(input)
                };
                let mut protected_wire = 0;
                let mut offers = Vec::new();
                for (publisher, encoding) in *protected {
                    let input = load(publisher, encoding, Some(1))?;
                    tracing::info!(
                        publisher,
                        encoding,
                        admitted = input.offers.len(),
                        elapsed,
                        wire_bps = input.wire_bytes as f64 * 8.0 / elapsed,
                        vcl_bps = input.vcl_bytes as f64 * 8.0 / elapsed,
                        "actual admitted T0/T1 source input"
                    );
                    protected_wire += input.wire_bytes;
                    offers.extend(input.offers);
                }
                // Reserve steady control capacity plus an entire control burst.
                // The phase has constant service, no held path or injected outage.
                let service_bps = capacity_bps
                    .checked_sub(8192)
                    .context("capacity below control reserve")?;
                let control_burst =
                    Duration::from_nanos(8_192_000_000_000u64.div_ceil(service_bps));
                let required_backlog =
                    super::provenance::required_input_backlog(&mut offers, service_bps)?
                        .checked_add(control_burst)
                        .context("input/control burst overflow")?;
                anyhow::ensure!(
                    required_backlog < *max_backlog,
                    "{description}: protected input train requires {required_backlog:?}, exceeds {max_backlog:?}"
                );
                let temporal_limit = match competing_tier {
                    SourceInputTier::Full => None,
                    SourceInputTier::T0T1 => Some(1),
                };
                let mut competing_vcl = 0;
                let mut burst = 1024;
                for (publisher, encoding) in *competing {
                    let input = load(publisher, encoding, temporal_limit)?;
                    tracing::info!(
                        publisher,
                        encoding,
                        ?competing_tier,
                        admitted = input.offers.len(),
                        elapsed,
                        "actual admitted competing source input"
                    );
                    competing_vcl += input.vcl_bytes;
                    burst += input.max_wire_au;
                }
                // Egress rotates a 60-byte SR/SDES once/second. Reserve 1KiB/s
                // for SRTCP, ICE and steady signaling too; padding is optional.
                let minimum_bps = protected_wire as f64 * 8.0 / elapsed + 8192.0;
                let competing_bps = competing_vcl as f64 * 8.0 / elapsed;
                anyhow::ensure!(
                    minimum_bps < *capacity_bps as f64,
                    "{description}: actual supplied protected upper load {minimum_bps} does not fit {capacity_bps}"
                );
                anyhow::ensure!(
                    competing_bps > *capacity_bps as f64,
                    "{description}: competing {competing_tier:?} coded slices alone {competing_bps} do not exceed {capacity_bps}"
                );
                anyhow::ensure!(
                    burst as f64 * 8.0 / (*capacity_bps as f64) < max_backlog.as_secs_f64(),
                    "{description}: synchronized competing AU burst does not fit declared backlog"
                );
                tracing::info!(
                    minimum_bps,
                    competing_bps,
                    ?competing_tier,
                    burst,
                    ?required_backlog,
                    capacity_bps,
                    "supplied-input-only joint feasibility verified"
                );
            }
            Step::CheckVideoQuality {
                description,
                participant,
                quality,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                let log = handle.video_rx();
                let native_generation = handle
                    .shared
                    .observation
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|observation| observation.snapshot.generation);
                if log.frames < quality.min_frames
                    || log
                        .decoder_errors
                        .saturating_sub(log.missing_parameter_set_keyframes)
                        > quality.max_decoder_errors
                    || log.ts_regression_count > 0
                    || log.non_contiguous > quality.max_non_contiguous
                {
                    for (name, peer) in handles.iter() {
                        let statistics = peer.shared.statistics.lock().unwrap();
                        match statistics.as_ref() {
                            Some(stats) => tracing::error!(
                                participant = name,
                                generation = stats.generation,
                                timestamp_us = stats.timestamp_us,
                                report = %String::from_utf8_lossy(&stats.report_json),
                                "native SDK statistics at video delivery failure"
                            ),
                            None => tracing::error!(
                                participant = name,
                                "native SDK statistics unavailable at video delivery failure"
                            ),
                        }
                    }
                    tracing::error!(native_queue_drops = log.native_queue_drops, receivers = ?log.by_receiver, "native receiver timelines at video delivery failure");
                }
                assert_video_quality(n, total, description, participant, quality, &log);
                if !quality.native_temporal_indices.is_empty() {
                    let generation = native_generation
                        .expect("native temporal witness requires a live graph")
                        .get();
                    log.require_native_temporal_indices(
                        generation,
                        quality.min_frames,
                        quality.native_temporal_indices,
                    )
                    .unwrap_or_else(|error| {
                        panic!("step {n}/{total} {kind}: {description} ({participant}): {error}")
                    });
                }
            }

            Step::CheckVideoQualityInterval {
                description,
                participant,
                quality,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                assert!(
                    !handle.video_rx().native_decoded_only(),
                    "encoded independent-decoder oracle cannot qualify native playout; use CheckNativeDecodedVideo"
                );
                let stats = handle.video_stats_since_interval();
                assert!(
                    stats.frames >= quality.min_frames,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {} frames in the last interval\n  actual:       frames={}, keyframes={}\n  note:         a cumulative count would still show every frame from before the\n                disturbance; this asks whether anything arrived after it",
                    quality.min_frames,
                    stats.frames,
                    stats.keyframes,
                );
                assert!(
                    stats.keyframes >= quality.min_keyframes,
                    "step {n}/{total} {kind}: {description} ({participant}) observed {} keyframes in the interval, minimum {}",
                    stats.keyframes,
                    quality.min_keyframes
                );
                assert!(
                    stats
                        .decoder_errors
                        .saturating_sub(stats.missing_parameter_set_keyframes)
                        <= quality.max_decoder_errors,
                    "step {n}/{total} {kind}: {description} ({participant}) observed {} decoder errors in the interval, maximum {}",
                    stats
                        .decoder_errors
                        .saturating_sub(stats.missing_parameter_set_keyframes),
                    quality.max_decoder_errors
                );
                assert!(
                    stats
                        .decoder_error_keyframes
                        .saturating_sub(stats.missing_parameter_set_keyframes)
                        <= quality.max_decoder_error_keyframes,
                    "step {n}/{total} {kind}: {description} ({participant}) observed {} undecodable keyframes in the interval, maximum {}",
                    stats
                        .decoder_error_keyframes
                        .saturating_sub(stats.missing_parameter_set_keyframes),
                    quality.max_decoder_error_keyframes
                );
                assert!(
                    stats.missing_parameter_set_keyframes
                        <= quality.max_missing_parameter_set_keyframes,
                    "step {n}/{total} {kind}: {description} ({participant}) observed {} keyframes missing parameter sets in the interval, maximum {}",
                    stats.missing_parameter_set_keyframes,
                    quality.max_missing_parameter_set_keyframes
                );
                assert_eq!(
                    stats.reference_mismatches, 0,
                    "step {n}/{total} {kind}: {description} ({participant}) had {} decoded frames without an exact corpus match",
                    stats.reference_mismatches
                );
                assert!(
                    stats.non_contiguous <= quality.max_non_contiguous,
                    "step {n}/{total} {kind}: {description} ({participant}) observed {} non-contiguous frames in the interval, maximum {}",
                    stats.non_contiguous,
                    quality.max_non_contiguous
                );
                assert_eq!(
                    stats.duplicate_ts_frames, 0,
                    "step {n}/{total} {kind}: {description} ({participant}) duplicated RTP timestamps in the interval"
                );
                assert_eq!(
                    stats.ts_regression_count, 0,
                    "step {n}/{total} {kind}: {description} ({participant}) regressed RTP timestamps in the interval"
                );
            }

            Step::CheckVideoNotReceivedFrom {
                description,
                participant,
                publisher,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant} <- {publisher})"
                );
                let publisher_id = handles
                    .get(publisher)
                    .ok_or_else(|| anyhow::anyhow!("step \"{description}\": unknown publisher {publisher}"))?
                    .participant_id()
                    .ok_or_else(|| {
                    anyhow::anyhow!(
                        "step \"{description}\": publisher {publisher} has no runtime participant id"
                    )
                })?;
                let handle = get_handle(handles, participant, description)?;
                handle
                    .video_rx()
                    .require_not_received_from(&publisher_id)
                    .unwrap_or_else(|error| {
                        panic!("step {n}/{total} {kind}: {description} ({participant} <- {publisher}): {error}")
                    });
            }

            Step::CheckVideoReceivedFrom {
                description,
                participant,
                publisher,
                min_frames,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant} <- {publisher})"
                );
                let publisher_id = handles
                    .get(publisher)
                    .ok_or_else(|| anyhow::anyhow!("step \"{description}\": unknown publisher {publisher}"))?
                    .participant_id()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "step \"{description}\": publisher {publisher} has no runtime participant id"
                        )
                    })?;
                let handle = get_handle(handles, participant, description)?;
                let actual = handle.video_rx().frames_from(&publisher_id);
                assert!(
                    actual >= *min_frames,
                    "step {n}/{total} {kind}: {description} ({participant} <- {publisher}): expected at least {min_frames} frames from participant_id {publisher_id}, got {actual}"
                );
            }

            Step::CheckVideoDecodedPublication {
                description,
                participant,
                publisher,
                min_frames,
            } => {
                let publisher = handles.get(publisher).ok_or_else(|| {
                    anyhow::anyhow!("step \"{description}\": unknown publisher {publisher}")
                })?;
                let connection = publisher
                    .shared
                    .observation
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|observation| observation.admission)
                    .ok_or_else(|| {
                        anyhow::anyhow!("step \"{description}\": publisher has no admission")
                    })?;
                let label = publisher.shared.video_label.lock().unwrap().clone();
                let source_inputs = publisher.shared.h264_publishers.clone();
                let track_id = connection
                    .derive_track_id(pulsebeam::entity::TrackKind::Video, &label)
                    .as_str();
                let receiver = get_handle(handles, participant, description)?;
                {
                    let observation = receiver.shared.observation.lock().unwrap();
                    let snapshot = &observation
                        .as_ref()
                        .context("publication receiver has no observation")?
                        .snapshot;
                    anyhow::ensure!(
                        snapshot.video_mapping.values().any(|id| id == &track_id)
                            && snapshot
                                .publications
                                .get(&track_id)
                                .is_some_and(|publication| {
                                    publication.kind
                                        == pulsebeam_agent_native::agent_core::MediaKind::Video
                                        && publication.label == label
                                }),
                        "{description}: receiver is not bound to publication {track_id} ({label})"
                    );
                }
                // Native track IDs identify receiver bindings, not supplied pictures.
                // Authenticate the caller's current source lifetime independently
                // of the separately checked catalog/mapping.
                let origin = super::provenance::current_source_key(
                    &source_inputs.lock().unwrap(),
                    &connection.to_string(),
                    "v0",
                )
                .context("publication has no constructed fixture source")?;
                receiver
                    .video_rx()
                    .require_decoded_from_incarnation(&origin, 0, *min_frames)
                    .with_context(|| {
                        format!("{description}: publication {track_id} ({label}) has no current-source decode")
                    })?;
            }

            Step::CheckVideoDecodedFrom {
                description,
                participant,
                publisher,
                min_frames,
                min_resolution,
            }
            | Step::CheckVideoDecodedFromInterval {
                description,
                participant,
                publisher,
                min_frames,
                min_resolution,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant} <- {publisher})"
                );
                let publisher_id = handles
                    .get(publisher)
                    .ok_or_else(|| anyhow::anyhow!("step \"{description}\": unknown publisher {publisher}"))?
                    .participant_id()
                    .ok_or_else(|| anyhow::anyhow!("step \"{description}\": publisher {publisher} has no runtime participant id"))?;
                let handle = get_handle(handles, participant, description)?;
                let baseline = if matches!(step, Step::CheckVideoDecodedFromInterval { .. }) {
                    handle
                        .interval_decoded_video_baseline
                        .get(&publisher_id)
                        .copied()
                        .unwrap_or(0)
                } else {
                    0
                };
                let observed = handle.video_rx();
                let decoded = observed
                    .require_decoded_from(&publisher_id, baseline, *min_frames)
                    .unwrap_or_else(|error| {
                        panic!("step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}): {error}")
                    });
                if matches!(step, Step::CheckVideoDecodedFromInterval { .. }) {
                    observed.require_decoded_size_since(
                        &publisher_id,
                        handle.interval_decoded_size_baseline.get(&publisher_id),
                        *min_frames,
                        *min_resolution,
                    ).unwrap_or_else(|error| {
                        panic!("step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}): {error}")
                    });
                } else {
                    assert!(
                        decoded.width >= min_resolution.0 && decoded.height >= min_resolution.1,
                        "step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}) decoded {}x{}, expected at least {}x{}",
                        decoded.width,
                        decoded.height,
                        min_resolution.0,
                        min_resolution.1
                    );
                }
                assert_eq!(
                    decoded.decoder_errors, 0,
                    "step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}) observed {} decoder errors",
                    decoded.decoder_errors
                );
                assert_eq!(
                    decoded.reference_mismatches, 0,
                    "step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}) had {} decoded frames without an exact corpus match",
                    decoded.reference_mismatches
                );
                if let Some((source, layer)) = handles
                    .get(publisher)
                    .and_then(|publisher_handle| publisher_handle.quality_video)
                {
                    assert_eq!(
                        (decoded.source, decoded.layer),
                        (Some(source), Some(layer)),
                        "step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}) decoded explicit corpus identity {:?}/{:?}, expected {source:?}/{layer:?}",
                        decoded.source,
                        decoded.layer
                    );
                }
                if decoded.reference_error.samples > 0 {
                    assert!(
                        decoded
                            .reference_error
                            .mean_absolute()
                            .is_some_and(|mean| mean <= MAX_REFERENCE_MEAN_ABSOLUTE),
                        "step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}) reference mean exceeds {MAX_REFERENCE_MEAN_ABSOLUTE}: {:?}",
                        decoded.reference_error
                    );
                    assert!(
                        decoded.reference_error.max <= MAX_REFERENCE_PEAK,
                        "step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}) reference peak exceeds {MAX_REFERENCE_PEAK}: {:?}",
                        decoded.reference_error
                    );
                    assert!(
                        decoded.source.is_some() && decoded.layer.is_some(),
                        "step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}) has pixels but no explicit corpus source/layer identity"
                    );
                }
            }

            Step::CheckVideoNotRenderedFrom {
                description,
                participant,
                publisher,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant} <- {publisher})"
                );
                let publisher_id = handles
                    .get(publisher)
                    .ok_or_else(|| anyhow::anyhow!("step \"{description}\": unknown publisher {publisher}"))?
                    .participant_id()
                    .ok_or_else(|| anyhow::anyhow!("step \"{description}\": publisher {publisher} has no runtime participant id"))?;
                let handle = get_handle(handles, participant, description)?;
                let decoded = handle.video_rx().decoded_from(&publisher_id);
                let Some(decoded) = decoded else {
                    panic!(
                        "step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}) has no decoder observation for a structurally complete corrupt frame"
                    );
                };
                assert_eq!(
                    decoded.frames, 0,
                    "step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}) rendered {} corrupt frames",
                    decoded.frames
                );
                assert!(
                    decoded.decoder_errors > 0,
                    "step {n}/{total} {kind}: {description} ({participant} <- {publisher_id}) reported no decoder error for corrupt H264"
                );
            }

            Step::CheckIdentityStable {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                let incarnations = handle.shared.incarnations.lock().unwrap().clone();
                assert_eq!(
                    incarnations.len(),
                    1,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     one identity for the whole call\n  actual:       {incarnations:?}\n  note:         it came back as a new participant rather than reconnecting, so\n                everyone else keeps a tile for the old one and gains a second"
                );
            }

            Step::CheckParticipantsKnown {
                description,
                participant,
                expected,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, {expected:?})"
                );
                let handle = get_handle(handles, participant, description)?;
                let known: BTreeSet<String> = handle
                    .shared
                    .discovered_tracks
                    .lock()
                    .unwrap()
                    .clone()
                    .into_iter()
                    .collect();
                let want: BTreeSet<String> = expected
                    .iter()
                    .filter_map(|name| {
                        handles
                            .get(name)
                            .and_then(|h| h.shared.participant_id.lock().unwrap().clone())
                    })
                    .collect();
                assert_eq!(
                    known, want,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     {expected:?}\n  still known:  {known:?}\n  note:         anyone here who has left is a ghost - a tile and a name for\n                somebody who is not in the room"
                );
            }

            Step::CheckSpeakerHeld {
                description,
                participant,
                speaker,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, speaker {speaker})"
                );
                let want = get_handle(handles, speaker, description)?
                    .shared
                    .participant_id
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "step \"{description}\": speaker \"{speaker}\" has no runtime participant id yet; add a Step::Run before this step"
                        )
                    })?;
                let handle = get_handle(handles, participant, description)?;
                let catalog = current_audio_catalog(handle)?;
                assert!(
                    catalog.values().any(|origin| origin == &want),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     {speaker} to hold a slot\n  signalled:    {catalog:?}"
                );
            }

            Step::CheckConnected {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                assert!(
                    handle.connected(),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     peer connection active\n  actual:       not connected"
                );
            }

            Step::CheckNotConnected {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                assert!(
                    !handle.connected(),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     no peer connection\n  actual:       connected"
                );
            }

            Step::CheckKeyframeRequests {
                description,
                participant,
                max,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, max {max})"
                );
                let handle = get_handle(handles, participant, description)?;
                let actual = handle.keyframe_requests();
                assert!(
                    actual <= *max,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≤ {max} keyframe (PLI) requests\n  actual:       {actual} (a climbing count means downstream cannot decode — PLI storm)"
                );
            }

            Step::CheckKeyframeRequestsAtLeast {
                description,
                participant,
                min,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, min {min})"
                );
                let handle = get_handle(handles, participant, description)?;
                let actual = handle.keyframe_requests();
                assert!(
                    actual >= *min,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {min} keyframe (PLI) requests\n  actual:       {actual}"
                );
            }

            Step::RecoverVideoAfterLostKeyframe {
                description,
                publisher,
                subscriber,
                encoding,
            } => {
                let source = get_handle(handles, publisher, description)?;
                *source.shared.keyframe_recovery.lock().unwrap() = KeyframeRecoveryProbe {
                    armed: true,
                    drop_target: Some(("v0".into(), Some((*encoding).into()))),
                    ..Default::default()
                };
                let viewer = get_handle(handles, subscriber, description)?;
                viewer.shared.keyframe_recovery.lock().unwrap().request_once = true;
            }

            Step::CheckKeyframeRecovery {
                description,
                publisher,
                subscriber,
                encoding,
            } => {
                let viewer = get_handle(handles, subscriber, description)?;
                assert_eq!(
                    viewer
                        .shared
                        .keyframe_recovery
                        .lock()
                        .unwrap()
                        .one_shot_requests,
                    1,
                    "{description}: receiver must send exactly one recovery PLI"
                );
                let log = viewer.video_rx();
                let decoded_baselines = viewer.interval_decoded_video_baseline.clone();
                let source = get_handle(handles, publisher, description)?;
                let probe = source.shared.keyframe_recovery.lock().unwrap();
                assert_eq!(
                    probe.discarded_responses, 1,
                    "{description}: exactly one response must be discarded"
                );
                let target = ("v0".to_owned(), Some((*encoding).to_owned()));
                assert_eq!(
                    probe.discarded_targets.as_slice(),
                    std::slice::from_ref(&target),
                    "{description}: only the selected encoding's first response is lost"
                );
                assert!(
                    probe
                        .requests
                        .iter()
                        .filter(|request| **request == target)
                        .count()
                        >= 2,
                    "{description}: selected slot/RID must be requested and retried; native sibling requests cannot substitute: {:?}",
                    probe.requests
                );
                let source_id = source.participant_id().expect("admitted source");
                let rendered = log
                    .decoded_from(&source_id)
                    .expect("the selected source must independently decode");
                let baseline = decoded_baselines.get(&source_id).copied().unwrap_or(0);
                assert!(
                    rendered.frames > baseline,
                    "{description}: recovery needs fresh decoded source output"
                );
                let expected = probe
                    .target_resolution
                    .expect("a native target request was observed");
                assert_eq!(
                    (rendered.width, rendered.height),
                    (usize::from(expected.0), usize::from(expected.1)),
                    "{description}: fresh decoding must be the actual selected encoding, not a native sibling"
                );
            }

            Step::CheckAdmissionFailed {
                description,
                participant,
                expected,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                assert_eq!(
                    *handle.shared.failed_admissions.lock().unwrap(),
                    *expected,
                    "{description}: failed application admission results"
                );
                assert!(
                    !handle.connected(),
                    "{description}: failed admission connected"
                );
                assert!(
                    handle.participant_id().is_none(),
                    "{description}: failed admission has an identity"
                );
            }

            Step::CheckRxBytes {
                description,
                participant,
                min_bytes,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, min {min_bytes} B)"
                );
                let handle = get_handle(handles, participant, description)?;
                let actual = handle.rx_bytes();
                assert!(
                    actual >= *min_bytes,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {min_bytes} bytes (cumulative)\n  actual:       {actual} bytes"
                );
            }

            Step::CheckNativeAudioDecoded {
                description,
                participant,
                min_samples,
                min_receivers,
                max_silence,
            } => {
                let handle = get_handle(handles, participant, description)?;
                require_native_audio(handle, *min_samples, *min_receivers, *max_silence)?;
            }

            Step::WaitForNativeAudioRecovery {
                description,
                participant,
                min_samples,
                timeout,
            } => {
                let handle = get_handle(handles, participant, description)?;
                let generation = audio_generation(handle)?;
                let waiting = tokio::time::timeout(*timeout, async {
                    while handle
                        .audio_rx()
                        .native_pcm
                        .require(
                            generation,
                            *min_samples,
                            1,
                            Some(Duration::from_millis(300)),
                            Instant::now(),
                        )
                        .is_err()
                    {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await;
                require_native_audio(handle, *min_samples, 1, Some(Duration::from_millis(300)))
                    .with_context(|| format!("{description}: native PCM recovery proof"))?;
                waiting.with_context(|| format!("{description}: native PCM recovery timed out"))?;
            }

            Step::CheckNativeAudioQuiet {
                description,
                participant,
                min_blocks,
                timeout,
            } => {
                let handle = get_handle(handles, participant, description)?;
                let generation = audio_generation(handle)?;
                let before = {
                    let statistics = handle.shared.statistics.lock().unwrap();
                    let stats = statistics
                        .as_ref()
                        .context("quiet audio baseline statistics unavailable")?;
                    anyhow::ensure!(
                        stats.generation == generation,
                        "quiet audio baseline belongs to an old generation"
                    );
                    stats.report_json.clone()
                };
                handle
                    .shared
                    .audio_rx
                    .lock()
                    .unwrap()
                    .native_pcm
                    .begin_window();
                let waiting = tokio::time::timeout(*timeout, async {
                    while require_native_quiet_audio(handle, generation, &before, *min_blocks)
                        .is_err()
                    {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await;
                require_native_quiet_audio(handle, generation, &before, *min_blocks).with_context(
                    || format!("{description}: fresh quiet PCM and decode progress"),
                )?;
                waiting.with_context(|| format!("{description}: quiet PCM window timed out"))?;
            }

            Step::CheckNativeAudioSilent {
                description,
                participant,
            } => {
                let handle = get_handle(handles, participant, description)?;
                let generation = audio_generation(handle)?;
                handle.audio_rx().native_pcm.require_silent(generation)?;
                let statistics = handle.shared.statistics.lock().unwrap();
                let stats = statistics
                    .as_ref()
                    .context("native silence requires actual native report")?;
                anyhow::ensure!(
                    stats.generation == generation,
                    "native silence report belongs to an old generation"
                );
                super::native_audio::require_no_audio(&stats.report_json)?;
            }

            Step::CheckEncodedAudio {
                description,
                participant,
                corrupt,
            }
            | Step::CheckEncodedAudioInterval {
                description,
                participant,
                corrupt,
            } => {
                let interval = matches!(step, Step::CheckEncodedAudioInterval { .. });
                let handle = get_handle(handles, participant, description)?;
                let generation = audio_generation(handle)?;
                let audio = handle.audio_rx();
                anyhow::ensure!(
                    audio.encoded_observers > 0,
                    "{description}: requires an attached encoded audio observer"
                );
                let streams: Vec<_> = audio
                    .by_receiver
                    .iter()
                    .filter(|(key, _)| key.generation == generation)
                    .map(|(_, stream)| stream)
                    .collect();
                anyhow::ensure!(
                    !streams.is_empty() && streams.iter().any(|stream| stream.packets > 0),
                    "{description}: no actual encoded Opus receiver packets"
                );
                anyhow::ensure!(
                    streams.iter().all(|stream| {
                        let span = if interval {
                            stream.packet_interval_span()
                        } else {
                            stream.packet_span()
                        };
                        span >= Duration::from_millis(300)
                    }),
                    "{description}: encoded receiver observation is only a startup transient"
                );
                if *corrupt {
                    anyhow::ensure!(
                        streams.iter().all(|stream| stream.decoded_samples == 0)
                            && streams.iter().any(|stream| stream.decoder_errors > 0),
                        "{description}: independent decoder accepted corrupted Opus: {streams:?}"
                    );
                } else {
                    anyhow::ensure!(
                        streams.iter().all(|stream| {
                            let span = if interval {
                                stream.decoded_interval_span()
                            } else {
                                stream.decoded_span()
                            };
                            span >= Duration::from_millis(300)
                                && stream.decoder_errors == 0
                                && stream.reference_mismatches == 0
                        }),
                        "{description}: receiver-scoped independent Opus decode failed: {streams:?}"
                    );
                }
                for stream in streams
                    .iter()
                    .filter(|stream| stream.reference_error.samples > 0)
                {
                    anyhow::ensure!(
                        stream
                            .reference_error
                            .mean_absolute()
                            .is_some_and(|mean| mean <= MAX_REFERENCE_MEAN_ABSOLUTE)
                            && stream.reference_error.max <= MAX_AUDIO_REFERENCE_PEAK,
                        "{description}: actual available Opus reference error exceeds bounds: {:?}",
                        stream.reference_error
                    );
                }
                tracing::info!(
                    ?streams,
                    "encoded receiver proof; unavailable reference comparisons are not errors or source identities"
                );
            }

            Step::CheckAudioSlotStreams {
                description,
                participant,
                expected_sources,
                max_streams,
            } => {
                let want = audio_expected(handles, expected_sources)?;
                let handle = get_handle(handles, participant, description)?;
                let generation = audio_generation(handle)?;
                let sources = handle.shared.interval_audio_sources.lock().unwrap().clone();
                anyhow::ensure!(
                    sources == want,
                    "{description}: public audio assignments {sources:?}, expected {want:?}"
                );
                let audio = handle.audio_rx();
                anyhow::ensure!(
                    audio.encoded_observers > 0,
                    "{description}: SSRC observations require encoded native metadata"
                );
                let streams: BTreeSet<_> = audio
                    .by_receiver
                    .keys()
                    .filter(|key| key.generation == generation)
                    .map(|key| key.ssrc)
                    .collect();
                anyhow::ensure!(
                    !streams.is_empty() && streams.len() <= *max_streams,
                    "{description}: expected 1..={max_streams} actual SSRC streams, got {streams:?}"
                );
                for (receiver, observed) in audio
                    .by_stream
                    .iter()
                    .filter(|(key, _)| key.generation == generation)
                {
                    tracing::info!(
                        ?receiver,
                        observed.packets,
                        observed.max_seq_gap,
                        "actual native encoded sequence observations; holes are not an audio-loss or playout verdict"
                    );
                }
            }

            Step::CheckAudioCatalogFrom {
                description,
                participant,
                expected,
            } => {
                let actual: BTreeSet<_> =
                    current_audio_catalog(get_handle(handles, participant, description)?)?
                        .into_values()
                        .collect();
                let want = audio_expected(handles, expected)?;
                anyhow::ensure!(
                    actual == want,
                    "{description}: current catalog assignments {actual:?}, expected {want:?}"
                );
            }

            Step::CheckTxBytes {
                description,
                participant,
                min_bytes,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, min {min_bytes} B)"
                );
                let handle = get_handle(handles, participant, description)?;
                let actual = handle.tx_bytes();
                assert!(
                    actual >= *min_bytes,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {min_bytes} bytes (cumulative)\n  actual:       {actual} bytes"
                );
            }

            Step::SetBandwidth {
                description,
                participant,
                bits_per_sec,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, {bits_per_sec} bps)"
                );
                let ip = name_to_ip.get(participant).copied().ok_or_else(|| {
                    anyhow::anyhow!("step \"{description}\": unknown participant {participant}")
                })?;
                pulsebeam_runtime::net::shaper::set_downlink(ip, *bits_per_sec);
            }

            Step::CheckMaxRxBytesInterval {
                description,
                participant,
                max_bytes,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, max {max_bytes} B)"
                );
                let handle = get_handle(handles, participant, description)?;
                let baseline = handle.interval_rx_baseline();
                let actual = handle.rx_bytes().saturating_sub(baseline);
                assert!(
                    actual <= *max_bytes,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     <= {max_bytes} bytes in last interval\n  actual:       {actual} bytes"
                );
            }

            Step::CheckRxBytesInterval {
                description,
                participant,
                min_bytes,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, min {min_bytes} B)"
                );
                let handle = get_handle(handles, participant, description)?;
                let baseline = handle.interval_rx_baseline();
                let actual = handle.rx_bytes().saturating_sub(baseline);
                assert!(
                    actual >= *min_bytes,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {min_bytes} bytes in last interval\n  actual:       {actual} bytes"
                );
            }

            Step::CheckTxBytesInterval {
                description,
                participant,
                min_bytes,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, min {min_bytes} B)"
                );
                let handle = get_handle(handles, participant, description)?;
                let baseline = handle.interval_tx_baseline();
                let actual = handle.tx_bytes().saturating_sub(baseline);
                assert!(
                    actual >= *min_bytes,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {min_bytes} bytes in last interval\n  actual:       {actual} bytes"
                );
            }

            Step::CheckDataReceived {
                description,
                participant,
                topic,
                expected,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, topic={topic})"
                );
                let handle = get_handle(handles, participant, description)?;
                let data_received = handle.shared.topic_events.lock().unwrap();
                let received = data_received
                    .get(*topic)
                    .map(std::vec::Vec::as_slice)
                    .unwrap_or(&[]);
                let received = received
                    .iter()
                    .filter_map(topic_payload)
                    .collect::<Vec<_>>();
                assert!(
                    received.contains(expected),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  topic:        {topic}\n  expected:     payload {expected:?} in received list\n  actual:       {received:?}"
                );
            }

            Step::CheckDataNotReceived {
                description,
                participant,
                topic,
                excluded,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, topic={topic})"
                );
                let handle = get_handle(handles, participant, description)?;
                let data_received = handle.shared.topic_events.lock().unwrap();
                let received = data_received
                    .get(*topic)
                    .map(std::vec::Vec::as_slice)
                    .unwrap_or(&[]);
                let received = received
                    .iter()
                    .filter_map(topic_payload)
                    .collect::<Vec<_>>();
                assert!(
                    !received.contains(excluded),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  topic:        {topic}\n  expected:     payload {excluded:?} NOT in received list\n  actual:       {received:?}"
                );
            }

            Step::CheckDataCount {
                description,
                participant,
                topic,
                expected,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, topic={topic})"
                );
                let handle = get_handle(handles, participant, description)?;
                let data_received = handle.shared.topic_events.lock().unwrap();
                let actual = data_received
                    .get(*topic)
                    .map_or(0, |events| events.iter().filter_map(topic_payload).count());
                assert_eq!(
                    actual, *expected,
                    "step {n}/{total} {kind}: {description} ({participant}, {topic}): expected exactly {expected} payloads, got {actual}"
                );
            }

            Step::CheckDataSequence {
                description,
                participant,
                topic,
                expected,
            } => {
                let handle = get_handle(handles, participant, description)?;
                let data_received = handle.shared.topic_events.lock().unwrap();
                let received = data_received
                    .get(*topic)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let received = received
                    .iter()
                    .filter_map(topic_payload)
                    .collect::<Vec<_>>();
                assert_eq!(
                    received.as_slice(),
                    *expected,
                    "step {n}/{total} {kind}: {description} ({participant}, {topic})"
                );
            }
            Step::CheckTopicIdentity {
                description,
                participant,
                topic,
                mode,
                publishers,
                gap,
            } => {
                let mut expected = BTreeMap::new();
                for publisher in *publishers {
                    let handle = get_handle(handles, publisher, description)?;
                    let observation = handle.shared.observation.lock().unwrap();
                    let id = observation
                        .as_ref()
                        .and_then(|observation| {
                            observation
                                .snapshot
                                .topics
                                .publishers
                                .iter()
                                .find(|status| {
                                    status.registration.topic == *topic
                                        && status.registration.mode == *mode
                                })
                                .and_then(|status| status.data_track_id.clone())
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "publisher {publisher} has no authoritative topic binding"
                            )
                        })?;
                    let participant_id = observation
                        .as_ref()
                        .and_then(|observation| {
                            observation.snapshot.participant_external_id.clone()
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "publisher {publisher} has no admitted external identity"
                            )
                        })?;
                    anyhow::ensure!(
                        expected.insert(participant_id, id).is_none(),
                        "duplicate admitted publisher identity"
                    );
                }
                let handle = get_handle(handles, participant, description)?;
                let events = handle.shared.topic_events.lock().unwrap();
                let mut next_sequence = BTreeMap::<String, u64>::new();
                let mut latest_sequence = BTreeMap::<String, u64>::new();
                let mut seen = BTreeSet::new();
                let mut gaps = Vec::new();
                for event in events.get(*topic).into_iter().flatten() {
                    match event {
                        TopicNotification::Message(message) if message.mode == *mode => {
                            anyhow::ensure!(
                                message.topic == *topic
                                    && expected.get(&message.participant_id)
                                        == Some(&message.data_track_id),
                                "{description}: message identity is not the publisher's authoritative track: {message:?}"
                            );
                            seen.insert(message.participant_id.clone());
                            if *mode == TopicMode::Latest {
                                if let Some(previous) = latest_sequence
                                    .insert(message.data_track_id.clone(), message.sequence)
                                {
                                    anyhow::ensure!(
                                        message.sequence > previous,
                                        "{description}: latest sequence regressed or duplicated"
                                    );
                                }
                            } else if *mode == TopicMode::Ordered {
                                if let Some(next) = next_sequence.get(&message.data_track_id) {
                                    anyhow::ensure!(
                                        message.sequence == *next,
                                        "{description}: ordered message skipped a sequence without an inline gap"
                                    );
                                }
                                next_sequence.insert(
                                    message.data_track_id.clone(),
                                    message
                                        .sequence
                                        .checked_add(1)
                                        .expect("valid wire sequence"),
                                );
                            }
                        }
                        TopicNotification::RecoveryGap {
                            subscriber,
                            data_track_id,
                            participant_id,
                            expected_sequence,
                            available_sequence,
                        } if *mode == TopicMode::Ordered => {
                            anyhow::ensure!(
                                subscriber.mode == *mode
                                    && expected.get(participant_id) == Some(data_track_id)
                                    && expected_sequence < available_sequence,
                                "{description}: invalid inline gap identity or range"
                            );
                            if let Some(next) = next_sequence.get(data_track_id) {
                                anyhow::ensure!(
                                    next == expected_sequence,
                                    "{description}: gap does not start at the last confirmed sequence"
                                );
                            }
                            next_sequence.insert(data_track_id.clone(), *available_sequence);
                            gaps.push((*expected_sequence, *available_sequence));
                        }
                        _ => {}
                    }
                }
                anyhow::ensure!(
                    seen == expected.keys().cloned().collect::<BTreeSet<_>>(),
                    "{description}: missing application messages from an expected publisher"
                );
                anyhow::ensure!(
                    gaps.as_slice() == gap.as_slice(),
                    "{description}: wrong inline gaps: {gaps:?}, expected {gap:?}"
                );
            }
        }
    }

    // Scoreboard. Emitted for every plan whether or not it asserted anything, so a change's
    // effect across the whole matrix is visible in one run rather than one scenario at a time,
    // and so a threshold can be chosen by observing what healthy looks like instead of guessing.
    //
    // Sorted by name: `handles` is a HashMap, and unordered output would make two runs diff
    // against each other for no reason.
    // Forwarded quality is recorded against the publisher's participant id, so resolve those
    // back to plan names once rather than per report.
    let quality_by_publisher: BTreeMap<&'static str, u8> = handles
        .iter()
        .filter_map(|(name, handle)| {
            let id = handle.participant_id()?;
            Some((*name, pulsebeam::sim_metrics::forwarded_quality(&id)?))
        })
        .collect();

    let changes_by_publisher: BTreeMap<&'static str, u64> = handles
        .iter()
        .filter_map(|(name, handle)| {
            let id = handle.participant_id()?;
            Some((*name, pulsebeam::sim_metrics::quality_changes(&id)))
        })
        .collect();

    let reversals_by_publisher: BTreeMap<&'static str, u64> = handles
        .iter()
        .filter_map(|(name, handle)| {
            let id = handle.participant_id()?;
            Some((*name, pulsebeam::sim_metrics::quality_reversals(&id)))
        })
        .collect();

    let mut names: Vec<_> = handles.keys().copied().collect();
    names.sort_unstable();
    for name in names {
        let (Some(handle), Some(ip)) = (handles.get(name), name_to_ip.get(name).copied()) else {
            continue;
        };
        // Publish-only participants never allocate downstream and have nothing to say.
        if handle.shared.participant_id.lock().unwrap().is_none() {
            continue;
        }
        tracing::info!(
            "[scoreboard] {name}: {}",
            report_metrics(handle, ip, window)
        );
        let mut report = measure(handle, ip, window);
        let video = handle.video_rx();
        report.decoded_video = handles
            .iter()
            .filter_map(|(name, publisher)| {
                let frames = publisher
                    .shared
                    .incarnations
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|id| video.decoded_by_publisher.get(id))
                    .map(|stream| stream.frames)
                    .sum::<u64>();
                (frames > 0).then_some((*name, frames))
            })
            .collect();
        report.forwarded_quality = quality_by_publisher.clone();
        report.quality_changes = changes_by_publisher.clone();
        report.quality_reversals = reversals_by_publisher.clone();
        reports
            .lock()
            .expect("reports poisoned")
            .insert(name, report);
    }

    // Signal all participants to stop.
    for handle in handles.values() {
        handle.send_command(ParticipantCmd::Done);
    }

    Ok(())
}

fn resolve(map: &PlanIps, name: &str, step_desc: &str) -> anyhow::Result<IpAddr> {
    map.get(name).copied().ok_or_else(|| {
        anyhow::anyhow!("step \"{step_desc}\": unknown participant/endpoint name \"{name}\"")
    })
}

fn get_handle<'a>(
    handles: &'a mut PlanHandles,
    name: &str,
    step_desc: &str,
) -> anyhow::Result<&'a mut ParticipantHandle> {
    handles
        .get_mut(name)
        .ok_or_else(|| anyhow::anyhow!("step \"{step_desc}\": unknown participant \"{name}\""))
}

fn expected_state(handles: &PlanHandles) -> anyhow::Result<super::oracle::ApplicationStateOracle> {
    let mut participants = Vec::new();
    for (name, handle) in handles {
        let allowed = handle.present
            || handle
                .retirement_deadline
                .is_some_and(|deadline| Instant::now() < deadline);
        if !allowed {
            continue;
        }
        let Some(id) = handle.participant_id() else {
            anyhow::ensure!(!handle.present, "{name} never completed admission");
            continue;
        };
        let observation = handle
            .shared
            .observation
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("{name} has no complete client observation"))?;
        let intent = handle.shared.pending_ops.lock().unwrap().intent.clone();
        participants.push(super::oracle::ExpectedParticipant {
            id: id.parse()?,
            room: handle.room_name,
            video: handle.publishes_video,
            video_label: handle.shared.video_label.lock().unwrap().clone(),
            audio: handle.publishes_audio,
            audio_pinned: intent.audio,
            automatic_audio: intent.automatic_audio,
            required: handle.present,
            connection: observation.admission,
            topics: intent.topics,
        });
    }
    Ok(super::oracle::ApplicationStateOracle { participants })
}

fn planned_video(
    participants: &[super::oracle::ExpectedParticipant],
    participant: &super::oracle::ExpectedParticipant,
    handle: &ParticipantHandle,
    observation: &ClientObservation,
) -> Vec<String> {
    let intent = handle.shared.pending_ops.lock().unwrap().intent.clone();
    let eligible = participants
        .iter()
        .filter(|peer| {
            peer.room == participant.room
                && peer.id != participant.id
                && peer.video
                && (peer.required
                    || observation
                        .snapshot
                        .participants
                        .contains_key(&peer.id.as_str()))
        })
        .map(|peer| peer.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut requested = intent
        .video
        .iter()
        .filter(|request| eligible.contains(&request.participant_id))
        .filter(|request| {
            request.publication.as_ref().is_none_or(|id| {
                participants.iter().any(|peer| {
                    peer.id.as_str() == request.participant_id
                        && peer
                            .track_id(pulsebeam::entity::TrackKind::Video, &peer.video_label)
                            .as_str()
                            == *id
                })
            })
        })
        .map(|request| request.participant_id.clone())
        .collect::<Vec<_>>();
    if handle.auto_subscribe {
        let automatic = eligible
            .iter()
            .filter(|id| !requested.contains(id))
            .cloned()
            .collect::<Vec<_>>();
        requested.extend(automatic);
    }
    requested.truncate(handle.video_capacity);
    requested
}

fn assert_room_state_consistent(handles: &PlanHandles, after: &str) {
    let expected = expected_state(handles)
        .unwrap_or_else(|error| panic!("room state after {after}: {error:#}"));
    for (name, handle) in handles.iter().filter(|(_, handle)| handle.present) {
        let allowed = expected
            .participants
            .iter()
            .filter(|peer| {
                peer.room == handle.room_name
                    && peer.id.as_str() != handle.participant_id().as_deref().unwrap_or_default()
            })
            .map(|peer| peer.id.as_str())
            .collect::<BTreeSet<_>>();
        let known = handle.shared.discovered_tracks.lock().unwrap();
        assert!(
            known.iter().all(|id| allowed.contains(id)),
            "stale or cross-room publication after {after}: observer={name}, known={known:?}, allowed={allowed:?}"
        );
    }
}

// Pinned str0m ICE-lite expires Binding Requests after 15 seconds and checks idle
// pairs every 3 seconds. This bounds detection plus control/shard retirement.
const CRASH_RETIREMENT_BUDGET: Duration = Duration::from_secs(20);

fn require_native_audio(
    handle: &ParticipantHandle,
    min_samples: u64,
    min_receivers: usize,
    max_silence: Option<Duration>,
) -> anyhow::Result<()> {
    let generation = audio_generation(handle)?;
    let tracks = handle.audio_rx().native_pcm.require(
        generation,
        min_samples,
        min_receivers,
        max_silence,
        Instant::now(),
    )?;
    let statistics = handle.shared.statistics.lock().unwrap();
    let stats = statistics
        .as_ref()
        .context("native audio statistics unavailable")?;
    anyhow::ensure!(
        stats.generation == generation,
        "native audio statistics belong to an old generation"
    );
    for track in tracks {
        super::native_audio::require_decoder_report(&stats.report_json, &track.mid)?;
        tracing::info!(
            ?track,
            min_samples,
            "native PCM decode verified without per-block publication or decoder-error claims"
        );
    }
    Ok(())
}

fn require_native_quiet_audio(
    handle: &ParticipantHandle,
    generation: u64,
    before: &[u8],
    min_blocks: u64,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        audio_generation(handle)? == generation,
        "quiet audio generation changed within measurement window"
    );
    let tracks =
        handle
            .audio_rx()
            .native_pcm
            .require_quiet(generation, min_blocks, 1, Instant::now())?;
    let statistics = handle.shared.statistics.lock().unwrap();
    let stats = statistics
        .as_ref()
        .context("quiet audio statistics unavailable")?;
    anyhow::ensure!(
        stats.generation == generation,
        "quiet audio statistics belong to an old generation"
    );
    for track in tracks {
        super::native_audio::require_decoder_progress(before, &stats.report_json, &track.mid)?;
        tracing::info!(
            ?track,
            min_blocks,
            "fresh quiet PCM with non-concealed native decode progress, without per-block publication attribution"
        );
    }
    Ok(())
}

fn audio_generation(handle: &ParticipantHandle) -> anyhow::Result<u64> {
    handle
        .shared
        .observation
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|observation| observation.snapshot.generation)
        .map(pulsebeam_agent_native::agent_core::Generation::get)
        .context("audio oracle requires a live native generation")
}

fn audio_expected(handles: &PlanHandles, expected: &[&str]) -> anyhow::Result<BTreeSet<String>> {
    expected
        .iter()
        .map(|name| {
            handles
                .get(name)
                .context("unknown expected audio publisher")?
                .participant_id()
                .context("audio publisher identity unavailable")
        })
        .collect()
}

fn current_audio_catalog(handle: &ParticipantHandle) -> anyhow::Result<BTreeMap<String, String>> {
    let observation = handle.shared.observation.lock().unwrap();
    let snapshot = &observation
        .as_ref()
        .context("audio catalog observation unavailable")?
        .snapshot;
    snapshot
        .audio_mapping
        .values()
        .map(|track| {
            let publication = snapshot
                .publications
                .get(track)
                .context("audio assignment has no catalog publication")?;
            Ok((
                track.as_str().to_owned(),
                publication.participant_id.as_str().to_owned(),
            ))
        })
        .collect()
}

async fn assert_convergence(handles: &PlanHandles, after: &str) -> anyhow::Result<()> {
    let check = || -> anyhow::Result<()> {
        let expected = expected_state(handles)?;
        for (name, handle) in handles.iter().filter(|(_, handle)| handle.present) {
            let observation = handle
                .shared
                .observation
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow::anyhow!("{name} has no client snapshot"))?;
            let id = handle
                .participant_id()
                .context("client has no public admission")?;
            let participant = expected
                .participants
                .iter()
                .find(|participant| participant.id.as_str() == id)
                .context("client has no bound application participant")?;
            let requested =
                planned_video(&expected.participants, participant, handle, &observation);
            expected
                .check_client(participant, &observation, &requested)
                .with_context(|| format!("public application state at {name}"))?;
        }
        Ok(())
    };
    let mut last = None;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match check() {
                Ok(()) => break,
                Err(error) => last = Some(error),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("application state did not converge after {after}: {last:?}"))
}

/// A `Step::Run` shorter than this is a pause mid-transition, not a settled room, so the state
/// invariant is not asked of it. Signalling a join or a departure is a round trip or two.
const ROOM_SETTLE_FLOOR: Duration = Duration::from_secs(3);

fn assert_video_quality(
    n: usize,
    total: usize,
    description: &str,
    participant: &str,
    quality: &VideoQuality,
    log: &VideoReceiveLog,
) {
    let kind = "CheckVideoQuality";
    assert!(
        !log.native_decoded_only(),
        "encoded independent-decoder oracle cannot qualify native playout; use CheckNativeDecodedVideo"
    );
    assert!(
        log.frames >= quality.min_frames,
        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {} decoded frames\n  actual:       frames={}, keyframes={}, decoder_errors={}, decoder_error_keyframes={}, missing_parameter_set_keyframes={}, ts_regression_count={}, non_contiguous={}",
        quality.min_frames,
        log.frames,
        log.keyframes,
        log.decoder_errors,
        log.decoder_error_keyframes,
        log.missing_parameter_set_keyframes,
        log.ts_regression_count,
        log.non_contiguous,
    );
    assert!(
        log.keyframes >= quality.min_keyframes,
        "step {n}/{total} {kind}: {description} ({participant}) observed {} keyframes, minimum {}",
        log.keyframes,
        quality.min_keyframes
    );
    assert!(
        log.decoder_errors
            .saturating_sub(log.missing_parameter_set_keyframes)
            <= quality.max_decoder_errors,
        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≤ {} decoder errors\n  actual:       frames={}, keyframes={}, decoder_errors={}, decoder_error_keyframes={}, ts_regression_count={}, non_contiguous={}",
        quality.max_decoder_errors,
        log.frames,
        log.keyframes,
        log.decoder_errors,
        log.decoder_error_keyframes,
        log.ts_regression_count,
        log.non_contiguous,
    );
    assert!(
        log.decoder_error_keyframes
            .saturating_sub(log.missing_parameter_set_keyframes)
            <= quality.max_decoder_error_keyframes,
        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≤ {} undecodable keyframes\n  actual:       frames={}, keyframes={}, decoder_errors={}, decoder_error_keyframes={}, ts_regression_count={}, non_contiguous={}",
        quality.max_decoder_error_keyframes,
        log.frames,
        log.keyframes,
        log.decoder_errors,
        log.decoder_error_keyframes,
        log.ts_regression_count,
        log.non_contiguous,
    );
    assert!(
        log.missing_parameter_set_keyframes <= quality.max_missing_parameter_set_keyframes,
        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≤ {} keyframes missing parameter sets\n  actual:       frames={}, keyframes={}, decoder_errors={}, decoder_error_keyframes={}, missing_parameter_set_keyframes={}, ts_regression_count={}, non_contiguous={}",
        quality.max_missing_parameter_set_keyframes,
        log.frames,
        log.keyframes,
        log.decoder_errors,
        log.decoder_error_keyframes,
        log.missing_parameter_set_keyframes,
        log.ts_regression_count,
        log.non_contiguous,
    );
    assert_eq!(
        log.reference_mismatches, 0,
        "step {n}/{total} {kind}: {description} ({participant}) had {} decoded frames without an exact corpus match",
        log.reference_mismatches
    );
    assert!(
        log.non_contiguous <= quality.max_non_contiguous,
        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≤ {} non-contiguous frames (gap budget)\n  actual:       frames={}, keyframes={}, decoder_errors={}, decoder_error_keyframes={}, ts_regression_count={}, non_contiguous={}",
        quality.max_non_contiguous,
        log.frames,
        log.keyframes,
        log.decoder_errors,
        log.decoder_error_keyframes,
        log.ts_regression_count,
        log.non_contiguous,
    );
    for (publisher, decoded) in &log.decoded_by_publisher {
        if decoded.reference_error.samples == 0 {
            continue;
        }
        assert!(
            decoded
                .reference_error
                .mean_absolute()
                .is_some_and(|mean| mean <= MAX_REFERENCE_MEAN_ABSOLUTE),
            "step {n}/{total} {kind}: {description} ({participant} <- {publisher}) decoded reference mean exceeds {MAX_REFERENCE_MEAN_ABSOLUTE}: {:?}",
            decoded.reference_error
        );
        assert!(
            decoded.reference_error.max <= MAX_REFERENCE_PEAK,
            "step {n}/{total} {kind}: {description} ({participant} <- {publisher}) decoded reference peak exceeds {MAX_REFERENCE_PEAK}: {:?}",
            decoded.reference_error
        );
    }
}

// ── LocalNodeSim ────────────────────────────────────────────────────────────

/// Receiver decoding or imposed-link queue/drop measurements.
/// Source identity, native I420 and presentation require their own proof.
#[derive(Clone, Copy, Debug)]
pub enum Property {
    /// Video frames decoded during the last run after a complete, parameterized keyframe.
    VideoDecodes,
    /// Mean bottleneck queueing delay measured by the imposed link shaper.
    QueueingDelayBelow(Duration),
    /// At most this percent of offered packets were dropped by a full bottleneck buffer.
    ///
    /// Congestion drops only, distinct from configured link loss: this is the controller
    /// overusing the link.
    CongestionLossBelow(u8),
}

fn pct(part: f64, whole: f64) -> f64 {
    if whole <= 0.0 {
        0.0
    } else {
        part / whole * 100.0
    }
}

/// Everything measurable about one participant's link over the last window.
/// Everything measured about one participant's link over a window.
///
/// Separate from its formatting on purpose. A property needs to *compute* over these numbers, and
/// a randomised scenario needs to return them rather than assert inline, so the measurement has
/// to exist as data. Rendering it for a human is a second, lesser job.
/// What a viewer actually experienced, as distinct from what the link carried.
///
/// One vocabulary, and below it one definition of the bar, because the alternative is what this
/// suite had: every plan inventing its own scalar check, none of them describing a picture. Bytes,
/// bitrates and layer indices are all invisible to a user. These are not.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Qoe {
    pub frames: u64,
    pub keyframes: u64,
    /// Keyframes without SPS+PPS. Each is a stretch the decoder could not render.
    pub undecodable_keyframes: u64,
    /// Frames preceded by a sequence hole: visible corruption rather than a clean picture.
    pub torn_frames: u64,
    /// From subscribing to the first frame on screen. `None` if nothing ever rendered.
    pub time_to_first_frame: Option<Duration>,
    pub longest_freeze: Duration,
    /// Time spent frozen as a fraction of the measured window.
    pub freeze_ratio: f64,
    pub mean_fps: f64,
}

/// What a viewer would say about a stream.
///
/// Deliberately three-valued. "Delivered / not delivered" is the distinction the suite used to
/// make, and it cannot express the failure that matters most: a stream that is technically
/// arriving and still unwatchable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Experience {
    /// Nothing ever rendered. The tile is blank.
    Blank,
    /// Rendered, but a viewer would call it broken. Carries which bar it missed.
    Broken(String),
    Watchable,
}

/// What the source is sending, which decides what "watchable" even means.
///
/// A framerate floor is right for a camera and wrong for a screen share: a still screen is
/// *supposed* to send almost nothing, and a bar that ignores that reports a defect on every
/// screenshare plan. The first run of this check did exactly that - 3.3 fps from a static share on
/// an 8 Mbps link, flagged as a slideshow when it was correct behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Content {
    /// A camera: continuous motion, so a framerate floor applies.
    Motion,
    /// A screen share: long still stretches are the normal case, so only freezes that a viewer
    /// would notice as *unresponsive* count, not a low frame count.
    Static,
}

impl Qoe {
    /// The bar, in one place.
    ///
    /// Every figure here is a perceptual claim rather than a tuning knob, each set where a viewer
    /// changes their mind about whether the call is working. Change them here and every plan and
    /// property moves together; that is the point of having one definition.
    pub fn experience(&self, content: Content) -> Experience {
        if self.frames == 0 {
            return Experience::Blank;
        }
        if self.undecodable_keyframes > 0 {
            return Experience::Broken(format!(
                "{} keyframes arrived without parameter sets, so they did not render",
                self.undecodable_keyframes
            ));
        }
        if let Some(ttff) = self.time_to_first_frame
            && ttff > MAX_TIME_TO_FIRST_FRAME
        {
            return Experience::Broken(format!(
                "first frame took {ttff:?}; a viewer has already decided it is broken"
            ));
        }
        // Freezes are only meaningful for a source that is supposed to be sending. A still screen
        // share is *supposed* to go quiet, and judging it by gaps flags correct behaviour on
        // every screenshare plan - the same mistake the framerate floor made before it learned
        // about content.
        if content == Content::Motion && self.longest_freeze > MAX_FREEZE {
            return Experience::Broken(format!(
                "froze for {:?} in one stretch",
                self.longest_freeze
            ));
        }
        if content == Content::Motion && self.freeze_ratio > MAX_FREEZE_RATIO {
            return Experience::Broken(format!(
                "spent {:.0}% of the window frozen",
                self.freeze_ratio * 100.0
            ));
        }
        if content == Content::Motion && self.mean_fps < MIN_FPS {
            return Experience::Broken(format!(
                "delivered {:.1} fps, which reads as a slideshow rather than video",
                self.mean_fps
            ));
        }
        Experience::Watchable
    }
}

/// A viewer waiting this long for a first frame has concluded the call is broken.
pub const MAX_TIME_TO_FIRST_FRAME: Duration = Duration::from_secs(5);
/// One unbroken stretch of stillness a viewer calls a freeze rather than a stutter.
pub const MAX_FREEZE: Duration = Duration::from_secs(3);
/// Fraction of a session that may be frozen before it is not a call.
pub const MAX_FREEZE_RATIO: f64 = 0.15;
/// Below this a camera reads as a slideshow. Well under any sane encoder target, so it catches a
/// stream that has fallen apart rather than one that shed a layer. Not applied to a screen share:
/// see [`Content`].
pub const MIN_FPS: f64 = 5.0;

#[derive(Clone, Debug, PartialEq)]
pub struct LinkReport {
    /// Configured capacity, and whether it held still for the window. Utilisation and the
    /// capacity-relative figures are only meaningful when it did.
    pub capacity_bps: Option<u64>,
    pub capacity_fixed: bool,
    pub window: Duration,
    /// Actual native interval bytes, unavailable without two fresh reports in
    /// the same generation. Diagnostics do not invent an initial zero sample.
    pub received_bytes: Option<u64>,
    pub forwarded_media_bytes: u64,
    pub decoded_video: BTreeMap<&'static str, u64>,
    /// Allocation passes recorded. Zero means the plan never exercised this participant, which a
    /// caller must not confuse with healthy behaviour.
    pub samples: usize,
    pub estimate_last_bps: u64,
    pub estimate_min_bps: u64,
    pub estimate_max_bps: u64,
    /// Largest fall from a running peak, as a percentage. The collapse measure.
    pub worst_drawdown_percent: f64,
    pub demand_last_bps: u64,
    pub demand_min_bps: u64,
    pub demand_max_bps: u64,
    pub max_backlog: Duration,
    /// The queue a packet typically waited behind, as distinct from the worst moment.
    pub standing_backlog: Duration,
    /// Longest gap between consecutive delivered video frames, once the stream had started.
    pub longest_silence: Duration,
    /// What this listener heard, per speaker.
    ///
    /// The SFU forwards only the loudest few speakers, so "was audio delivered" is the wrong
    /// question and "from whom" is the right one. A total byte count cannot distinguish the
    /// selector picking correctly from it picking at all.
    pub audio: AudioReceiveLog,
    /// What the decoder made of the stream, as distinct from what the link carried.
    ///
    /// Every other figure here is about bytes and bitrates, and a viewer cannot see bytes. A
    /// stream can arrive at the right rate, on the right layer, and still render nothing: a
    /// keyframe without its parameter sets is not decodable, and the picture stays blank while
    /// every byte-level measure looks healthy.
    pub qoe: Qoe,
    pub delivered_packets: u64,
    pub congestion_drops: u64,
    pub link_loss_drops: u64,
    /// How many times each publisher's forwarded layer changed during the window.
    ///
    /// The measure that catches instability. Every other figure here is an average or an
    /// endpoint, and a stream flipping between two layers many times a second looks perfectly
    /// healthy in all of them - right final layer, right byte count, right estimate.
    pub quality_changes: BTreeMap<&'static str, u64>,
    /// How many times each publisher's forwarded layer *reversed direction* during the window.
    ///
    /// Climbing through layers as bandwidth is discovered is correct, so a raw change count cannot
    /// separate a healthy ramp from a stream oscillating between two - it counts both. A reversal
    /// is the part that is never right, which makes it the figure a property can assert on across
    /// a whole run rather than only after everything has settled.
    pub quality_reversals: BTreeMap<&'static str, u64>,
    /// Layer last forwarded from each publisher, by participant name. 0 means paused.
    ///
    /// Keyed by the *publisher* rather than folded into the link's numbers, because contention is
    /// a claim about how several streams fared against each other. A viewer receiving plenty of
    /// bytes tells you nothing about whether one of its two streams was starved to pay for the
    /// other.
    pub forwarded_quality: BTreeMap<&'static str, u8>,
}

impl LinkReport {
    /// Delivered throughput as a percentage of what the link could have carried. `None` unless
    /// the capacity held still, since otherwise there is no single denominator.
    pub fn utilisation_percent(&self) -> Option<f64> {
        let capacity = self.capacity_bps.filter(|_| self.capacity_fixed)?;
        let deliverable = capacity as f64 * self.window.as_secs_f64() / 8.0;
        Some(pct(self.received_bytes? as f64, deliverable))
    }

    /// Congestion tail-drop as a percentage of packets offered to the bottleneck. Distinct from
    /// configured link loss: this is the controller overusing the link.
    pub fn congestion_loss_percent(&self) -> f64 {
        let offered = self.delivered_packets + self.congestion_drops;
        pct(self.congestion_drops as f64, offered as f64)
    }

    /// SFU forwarded-byte / received-transport-byte ratio, for diagnostics only.
    /// This is not receiver efficiency and can exceed 100% under loss.
    #[allow(dead_code)] // Report accessor, kept alongside the rest of the surface.
    pub fn media_percent(&self) -> Option<f64> {
        Some(pct(
            self.forwarded_media_bytes as f64,
            self.received_bytes? as f64,
        ))
    }

    /// What the controller ought to have found: the lesser of what was asked for and what the
    /// link could give. The reference that stays meaningful in both regimes.
    pub fn need_bps(&self) -> u64 {
        self.demand_last_bps
            .min(self.capacity_bps.unwrap_or(u64::MAX))
    }
}

/// Derive what the viewer experienced. One place, so the scoreboard and the assertions can never
/// disagree about what was measured.
fn qoe_of(handle: &ParticipantHandle, _window: Duration) -> Qoe {
    // Session-scoped, deliberately, where the rest of the report is window-scoped. A viewer
    // experiences a call, not a measurement window: a freeze in an earlier step still happened to
    // them, and time-to-first-frame has no meaning inside a later window at all. Mixing the two
    // reported a 5s freeze as 0% of the session, because a cumulative maximum sat next to a
    // per-window total.
    let video = handle.video_rx().stats();
    // The span the stream was actually live for, taken from the frame timestamps themselves. A
    // clock read on the coordinator belongs to a different host's epoch and gave a denominator
    // that silently swallowed every freeze.
    let seconds = video
        .first_frame_at
        .zip(video.last_frame_at)
        .map(|(first, last)| last.saturating_duration_since(first).as_secs_f64())
        .unwrap_or(0.0)
        .max(f64::EPSILON);
    Qoe {
        frames: video.frames,
        keyframes: video.keyframes,
        undecodable_keyframes: video.decoder_error_keyframes,
        torn_frames: video.non_contiguous,
        // Only where the plan issued an explicit subscription. Falling back to the moment the
        // participant was created measured the plan's own scaffolding instead: nearly every plan
        // opens with a five-second "establish connection" step, and that reference put the median
        // time-to-first-frame at 5.18s across the whole suite - an artefact, not a product
        // latency.
        time_to_first_frame: handle
            .subscribed_at
            .zip(video.first_frame_at)
            .map(|(asked, shown)| shown.saturating_duration_since(asked)),
        longest_freeze: video.longest_frame_gap,
        freeze_ratio: (video.frozen_time.as_secs_f64() / seconds).clamp(0.0, 1.0),
        mean_fps: video.frames as f64 / seconds,
    }
}

fn measure(handle: &ParticipantHandle, ip: IpAddr, window: Duration) -> LinkReport {
    let now = tokio::time::Instant::now();
    let stats = pulsebeam_runtime::net::shaper::stats(ip);
    let series = handle
        .participant_id()
        .map(|id| pulsebeam::sim_metrics::bwe_series(&id))
        .unwrap_or_default();

    let mut peak = 0.0f64;
    let mut drawdown = 0.0f64;
    for (_, bps, _) in &series {
        peak = peak.max(*bps as f64);
        if peak > 0.0 {
            drawdown = drawdown.max((peak - *bps as f64) / peak * 100.0);
        }
    }

    let qoe = qoe_of(handle, window);

    LinkReport {
        capacity_bps: pulsebeam_runtime::net::shaper::capacity_at(ip, now),
        capacity_fixed: pulsebeam_runtime::net::shaper::capacity_is_fixed(ip),
        window,
        received_bytes: handle.interval_delta(|stats| stats.bytes_received).ok(),
        forwarded_media_bytes: handle.forwarded_media(),
        decoded_video: BTreeMap::new(),
        samples: series.len(),
        estimate_last_bps: series.last().map(|s| s.1).unwrap_or(0),
        estimate_min_bps: series.iter().map(|(_, b, _)| *b).min().unwrap_or(0),
        estimate_max_bps: series.iter().map(|(_, b, _)| *b).max().unwrap_or(0),
        worst_drawdown_percent: drawdown,
        demand_last_bps: series.last().map(|s| s.2).unwrap_or(0),
        demand_min_bps: series.iter().map(|(_, _, d)| *d).min().unwrap_or(0),
        demand_max_bps: series.iter().map(|(_, _, d)| *d).max().unwrap_or(0),
        max_backlog: stats.max_backlog,
        standing_backlog: stats.mean_backlog(),
        longest_silence: qoe.longest_freeze,
        qoe,
        audio: handle.audio_rx(),
        delivered_packets: stats.delivered,
        congestion_drops: stats.dropped_overflow,
        link_loss_drops: stats.dropped_loss,
        forwarded_quality: BTreeMap::new(),
        quality_changes: BTreeMap::new(),
        quality_reversals: BTreeMap::new(),
    }
}

fn report_metrics(handle: &ParticipantHandle, ip: IpAddr, window: Duration) -> String {
    let qoe_now = qoe_of(handle, window);
    let now = tokio::time::Instant::now();
    let stats = pulsebeam_runtime::net::shaper::stats(ip);
    let capacity = pulsebeam_runtime::net::shaper::capacity_at(ip, now);
    let received = handle.interval_delta(|stats| stats.bytes_received);
    let forwarded = handle.forwarded_media();

    let series = handle
        .shared
        .participant_id
        .lock()
        .unwrap()
        .clone()
        .map(|id| pulsebeam::sim_metrics::bwe_series(&id))
        .unwrap_or_default();

    let mut out = String::new();
    let fixed = pulsebeam_runtime::net::shaper::capacity_is_fixed(ip);
    match (capacity, received.as_ref()) {
        (capacity, Err(reason)) => out.push_str(&format!(
            "capacity {capacity:?} bps | received unavailable ({reason})"
        )),
        // Utilisation needs a capacity that held for the window. On a schedule the instantaneous
        // value is the wrong denominator and produces nonsense - a ramp ending at 700 kbps whose
        // window averaged ~1.6 Mbps reported 137% - so report the rate and omit the ratio.
        (Some(c), Ok(received)) if !fixed => out.push_str(&format!(
            "capacity {c} bps (scheduled) | received {received} B"
        )),
        (Some(c), Ok(received)) => {
            let deliverable = c as f64 * window.as_secs_f64() / 8.0;
            out.push_str(&format!(
                "capacity {c} bps | utilisation {:.1}% ({received} B / ~{deliverable:.0} B)",
                pct(*received as f64, deliverable)
            ));
        }
        (None, Ok(received)) => out.push_str(&format!("capacity unshaped | received {received} B")),
    }

    if series.is_empty() {
        out.push_str(" | estimate: no allocation passes");
    } else {
        let last = series.last().expect("non-empty").1;
        let min = series.iter().map(|(_, b, _)| *b).min().unwrap_or(0);
        let max = series.iter().map(|(_, b, _)| *b).max().unwrap_or(0);

        let mut peak = 0.0f64;
        let mut drawdown = 0.0f64;
        for (_, bps, _) in &series {
            peak = peak.max(*bps as f64);
            if peak > 0.0 {
                drawdown = drawdown.max((peak - *bps as f64) / peak * 100.0);
            }
        }
        // Demand alongside the estimate: the probe target is derived from it, so a demand that
        // collapses drags the estimate down with it regardless of what the link can carry.
        let d_last = series.last().expect("non-empty").2;
        let d_min = series.iter().map(|(_, _, d)| *d).min().unwrap_or(0);
        let d_max = series.iter().map(|(_, _, d)| *d).max().unwrap_or(0);
        out.push_str(&format!(
            " | estimate last {last} min {min} max {max} bps, worst drawdown {drawdown:.1}%, \
             {} samples | demand last {d_last} min {d_min} max {d_max} bps",
            series.len()
        ));

        if let Some(c) = capacity {
            let off = (last as f64 - c as f64).abs() / c as f64 * 100.0;
            out.push_str(&format!(" | ends {off:.1}% off capacity"));
            for target in [80u8, 90] {
                let want = c as f64 * target as f64 / 100.0;
                match series.iter().find(|(_, b, _)| *b as f64 >= want) {
                    Some((at, _, _)) => out.push_str(&format!(" | {target}% at {at:?}")),
                    None => out.push_str(&format!(" | {target}% never")),
                }
            }
        }
    }

    let offered = stats.delivered + stats.dropped_overflow;
    out.push_str(&format!(
        " | qoe {} fps={:.1} key={} undecodable={} torn={} ttff={:?} freeze={:?}/{:.0}% | queue standing {:?} max {:?} | congestion loss {:.2}% ({}/{offered}) | link loss {} | forwarded/rx {}",
        qoe_now.frames,
        qoe_now.mean_fps,
        qoe_now.keyframes,
        qoe_now.undecodable_keyframes,
        qoe_now.torn_frames,
        qoe_now.time_to_first_frame,
        qoe_now.longest_freeze,
        qoe_now.freeze_ratio * 100.0,
        stats.mean_backlog(),
        stats.max_backlog,
        pct(stats.dropped_overflow as f64, offered as f64),
        stats.dropped_overflow,
        stats.dropped_loss,
        match received {
            Ok(bytes) => format!("{:.1}%", pct(forwarded as f64, bytes as f64)),
            Err(reason) => format!("unavailable ({reason})"),
        },
    ));
    out
}

/// Evaluate `property`, returning a human-readable reason on failure.
fn check_property(
    property: &Property,
    handle: &ParticipantHandle,
    ip: IpAddr,
) -> Result<(), String> {
    let stats = pulsebeam_runtime::net::shaper::stats(ip);

    match *property {
        Property::VideoDecodes => {
            let total = handle.video_rx().stats();
            let video = total.since(handle.interval_video_baseline);
            if video.frames == 0 {
                return Err("no video frames were decoded during the run".to_string());
            }
            if total.keyframes == 0 {
                return Err("no video keyframe was decoded".to_string());
            }
            if total.decoder_error_keyframes != 0 {
                return Err(format!(
                    "{} decoded keyframes were missing parameter sets",
                    total.decoder_error_keyframes
                ));
            }
        }
        Property::QueueingDelayBelow(max) => {
            let standing = stats.mean_backlog();
            if standing > max {
                return Err(format!(
                    "bottleneck queue stood at {standing:?}; expected below {max:?} \
                     (peaked at {:?})",
                    stats.max_backlog
                ));
            }
        }
        Property::CongestionLossBelow(max) => {
            let offered = stats.delivered + stats.dropped_overflow;
            let got = pct(stats.dropped_overflow as f64, offered as f64);
            if got > max as f64 {
                return Err(format!(
                    "{got:.1}% of offered packets were dropped by a full buffer \
                     ({} of {offered}); expected below {max}%",
                    stats.dropped_overflow
                ));
            }
        }
    }
    Ok(())
}

/// Characteristics of the simulated link between every pair of hosts.
///
/// **This matters enormously for congestion control.** turmoil's own default is a uniform
/// random 0-100ms latency applied independently per message. That is not a network: it has no
/// ordering correlation, so a burst of packets sent 2ms apart arrives smeared over 100ms in
/// arbitrary order.
///
/// Inter-packet arrival spacing is precisely the signal GCC measures. Under turmoil's default,
/// every probe under-reads its own send rate by 2-4x, the trendline estimator sees pure noise,
/// and reordering is charged as packet loss - the loss controller settles on an 8-9% inherent
/// loss estimate on a link that drops nothing. Any BWE conclusion drawn in that environment is
/// an artifact of the simulator.
///
/// These profiles instead use a tight latency band, which is what a real path looks like:
/// propagation delay is near-constant and jitter is small relative to it.
///
/// Note the jitter band must stay narrow even for "bad" networks. turmoil draws each message's
/// latency *independently*, so a +/-15ms band reorders roughly 30 consecutive packets - far more
/// than a real link, where packets queue in order behind one another. Measured against a 1% loss
/// configuration that produced 17-51% apparent loss, because the receiver counts not-yet-arrived
/// packets as lost. Model a worse network by raising latency and loss, not by widening jitter.
///
/// Capacity, buffer depth and the loss model live in `pulsebeam_runtime::net::shaper`; turmoil
/// itself has no notion of any of them.
#[derive(Debug, Clone, Copy)]
pub struct LinkProfile {
    pub min_latency: Duration,
    pub max_latency: Duration,
    /// Fraction of SFU-egress datagrams dropped outright, 0.0..=1.0.
    pub loss: f64,
    /// Downlink capacity per participant, in bits per second. `None` leaves the path unlimited.
    ///
    /// **Only set this when the rate is meant to bind.** Release is timer-driven, but it still
    /// quantises to the granularity the runtime can schedule at. When the shaped rate is the
    /// dominant delay that quantisation is lost in the serialisation delay and the model is sound;
    /// on a link fast enough that serialisation is negligible it becomes the largest source of
    /// inter-packet jitter, which is precisely the signal GCC measures. Shaping a non-binding link
    /// therefore manufactures congestion rather than removing an artifact:
    /// measured, a plan delivering 2 MB unshaped delivered 721 kB at both 50 Mbps *and* 1 Gbps —
    /// identical, so the limit was the shaper rather than the link.
    ///
    /// The cost of leaving it `None` is that the plan says nothing about congestion control: with
    /// nothing to saturate, the estimate climbs to `MAX_BANDWIDTH` and sits there (measured at
    /// exactly 5,000,000 bps, 0.0% drawdown). That is the right trade for plans about allocation
    /// or signalling, and the wrong one for plans about the estimator.
    ///
    /// turmoil has no notion of capacity, so without this a simulated path carries any offered
    /// load: there is no queueing delay, the delay-based estimator never backs off, and a probe
    /// that under-delivers cannot pull the estimate down. Set it to make congestion real. See
    /// `pulsebeam_runtime::net::shaper`.
    pub bandwidth_bps: Option<u64>,
    /// Loss model for the path, replacing `loss` when set. Real wireless loses in bursts, and a
    /// controller tuned against the same average spread evenly is not tested against it.
    pub loss_model: Option<Loss>,
    /// Out-of-order delivery on the path.
    pub reorder: Reorder,
    /// Fraction of datagrams delivered twice, 0.0..=1.0.
    pub duplicate: f64,
    /// Impairment applied to the *client to SFU* direction, which carries transport feedback.
    ///
    /// Loss, reordering and duplication are configured per destination, and every plan until now
    /// only configured the participants' addresses - so feedback reached the SFU perfectly however
    /// bad the path to the viewer was. Congestion control is a closed loop: an estimator whose
    /// feedback is assumed lossless has not been tested on a real network. `None` leaves the
    /// return path clean, which is the right choice only when a plan is deliberately isolating
    /// the forward direction.
    pub feedback: Option<FeedbackProfile>,
    /// How long the receiver may coalesce same-source datagrams into one GRO
    /// batch.
    ///
    /// A NAPI poll interval, so it belongs to the NIC rather than to the shard
    /// scheduler — pinning it to the timer quantum only looked right while the
    /// two happened to share a value.
    pub gro_window: Duration,
}

/// See [`LinkProfile::gro_window`].
pub const GRO_WINDOW: Duration = Duration::from_micros(100);

/// Impairment on the path carrying transport feedback back to the SFU.
#[derive(Clone, Copy, Debug, Default)]
pub struct FeedbackProfile {
    pub loss: Option<Loss>,
    pub reorder: Reorder,
}

fn topic_payload(event: &TopicNotification) -> Option<&[u8]> {
    match event {
        TopicNotification::Message(message) => Some(&message.payload),
        _ => None,
    }
}

fn initial_topic_registrations(plan: &[Step]) -> BTreeMap<&'static str, TopicRegistrations> {
    // Data-channel labels are transport topology; scenario steps still control when each API
    // declaration becomes observable, but fixed labels belong in the initial offer.
    let mut by_participant = BTreeMap::<_, TopicRegistrations>::new();
    for step in plan {
        let (participant, publisher, subscriber) = match step {
            Step::DeclarePublishTopic {
                participant,
                topic,
                mode,
                ..
            } => (
                *participant,
                Some(TopicPublisher {
                    topic: (*topic).to_string(),
                    mode: *mode,
                }),
                None,
            ),
            Step::DeclareSubscribeTopic {
                participant,
                topic,
                mode,
                ..
            } => (
                *participant,
                None,
                Some(TopicSubscriber {
                    topic: (*topic).to_string(),
                    mode: *mode,
                }),
            ),
            _ => continue,
        };
        let registrations = by_participant.entry(participant).or_default();
        if let Some(publisher) = publisher
            && !registrations.publishers.contains(&publisher)
        {
            registrations.publishers.push(publisher);
        }
        if let Some(subscriber) = subscriber
            && !registrations.subscribers.contains(&subscriber)
        {
            registrations.subscribers.push(subscriber);
        }
    }
    by_participant
}

impl FeedbackProfile {
    /// Feedback degraded about as much as the forward path on the same wireless link.
    pub fn wifi() -> Self {
        Self {
            loss: Some(Loss::wifi()),
            reorder: Reorder::occasional(),
        }
    }

    /// Mobile uplink: feedback is scarcer and later than on the downlink.
    pub fn cellular() -> Self {
        Self {
            loss: Some(Loss::cellular()),
            reorder: Reorder::occasional(),
        }
    }
}

impl LinkProfile {
    /// Wired/fibre: ~10ms RTT, minimal jitter, no loss. The default.
    pub fn fiber() -> Self {
        Self {
            min_latency: Duration::from_millis(5),
            max_latency: Duration::from_millis(6),
            loss: 0.0,
            bandwidth_bps: None,
            loss_model: None,
            reorder: Reorder::NONE,
            duplicate: 0.0,
            feedback: None,
            gro_window: GRO_WINDOW,
        }
    }

    /// Home Wi-Fi: a little more latency and jitter than wired, plus occasional loss.
    pub fn wifi() -> Self {
        Self {
            min_latency: Duration::from_millis(8),
            max_latency: Duration::from_millis(13),
            loss: 0.002,
            bandwidth_bps: None,
            loss_model: Some(Loss::wifi()),
            reorder: Reorder::occasional(),
            duplicate: 0.0005,
            feedback: Some(FeedbackProfile::wifi()),
            gro_window: GRO_WINDOW,
        }
    }

    /// Mobile: markedly higher latency and 1% loss, with jitter kept narrow (see the note above
    /// on why a wide band is not a realistic way to model a worse network).
    pub fn cellular() -> Self {
        Self {
            min_latency: Duration::from_millis(45),
            max_latency: Duration::from_millis(52),
            loss: 0.01,
            bandwidth_bps: None,
            loss_model: Some(Loss::cellular()),
            reorder: Reorder::occasional(),
            duplicate: 0.001,
            feedback: Some(FeedbackProfile::cellular()),
            gro_window: GRO_WINDOW,
        }
    }
}

impl Default for LinkProfile {
    /// Wi-Fi, not fibre.
    ///
    /// A plan that says nothing about its link is not asking for a perfect one - it is asking for
    /// a normal one, and normal is a home Wi-Fi path that loses packets in bursts, occasionally
    /// delivers late, and sometimes delivers twice. Defaulting to fibre meant almost the whole
    /// suite validated behaviour against conditions no user has, which is how a probe regression
    /// that only appears under jitter and burst loss passed every plan here. Opt into
    /// [`LinkProfile::fiber`] when a plan is about signalling or allocation logic and the path is
    /// deliberately not the variable under test.
    fn default() -> Self {
        Self::wifi()
    }
}

pub struct LocalNodeSim {
    rooms: Vec<Room>,
    tick_duration: Duration,
    rng_seed: u64,
    subnet: Option<u8>,
    tcp_only: bool,
    udp_only: bool,
    num_shards: usize,
    link: LinkProfile,
    ip_version: turmoil::IpVersion,
    predeclare_topics: bool,
}

impl Default for LocalNodeSim {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalNodeSim {
    pub fn new() -> Self {
        Self {
            rooms: Vec::new(),
            tick_duration: pulsebeam_runtime::SHARD_TIMER_QUANTUM,
            rng_seed: super::sim_seed(),
            subnet: None,
            tcp_only: false,
            // The shaper models datagrams. Keep its default path truthful and
            // exercise reliable-stream framing only in explicit TCP scenarios.
            udp_only: true,
            num_shards: DEFAULT_SIM_SHARDS,
            link: LinkProfile::default(),
            ip_version: turmoil::IpVersion::V4,
            predeclare_topics: true,
        }
    }

    /// Submit topic declarations during the plan instead of the initial offer.
    pub fn without_initial_topics(mut self) -> Self {
        self.predeclare_topics = false;
        self
    }

    /// Set the simulated link characteristics. See [`LinkProfile`].
    pub fn with_link(mut self, link: LinkProfile) -> Self {
        self.link = link;
        self
    }

    /// Cap each participant's downlink. See [`LinkProfile::bandwidth_bps`].
    pub fn with_bandwidth(mut self, bits_per_sec: u64) -> Self {
        self.link.bandwidth_bps = Some(bits_per_sec);
        self
    }

    pub fn with_room(mut self, r: Room) -> Self {
        self.rooms.push(r);
        self
    }

    /// Pin the subnet this plan runs on, rather than taking the next one available.
    ///
    /// Addresses are otherwise handed out by a process-wide counter, so a plan's IPs depend on
    /// how many plans ran before it. That is harmless when every plan is its own process, and
    /// quietly destroys reproducibility when they are not: proptest runs all of its cases in one
    /// process, draws a different sample each run, and so the *same* scenario lands on different
    /// addresses depending on where in the sample it fell. A case that failed then passed on
    /// replay, which is the one thing a randomised suite may not do.
    ///
    /// Callers deriving this from their input get addresses fixed by the scenario itself. Only
    /// safe because plans within a process run one at a time; two concurrent plans on one subnet
    /// would share a network.
    pub fn with_subnet(mut self, subnet: u8) -> Self {
        self.subnet = Some(1 + (subnet % 200));
        self
    }

    #[allow(unused)]
    pub fn with_rng_seed(mut self, seed: u64) -> Self {
        self.rng_seed = seed;
        self
    }

    /// Use TCP-only SFU + TCP client connections.
    pub fn with_tcp_only(mut self) -> Self {
        self.tcp_only = true;
        self.udp_only = false;
        self
    }

    /// Advertise only UDP candidates (the simulation default).
    pub fn with_udp_only(mut self) -> Self {
        self.udp_only = true;
        self.tcp_only = false;
        self
    }

    /// Run the complete node and client scenario over Turmoil's IPv6 network.
    /// The default remains IPv4 so existing scenario names stay concise; this
    /// switch changes every bound and advertised address together.
    pub fn with_ipv6(mut self) -> Self {
        self.ip_version = turmoil::IpVersion::V6;
        self
    }

    /// Spread the node across N worker shards.
    ///
    /// Independent of [`Self::with_tcp_only`]. Over UDP the shard a datagram
    /// reaches is chosen by the `SO_REUSEPORT` group, which hashes its 4-tuple
    /// exactly as the kernel does — so a plan that sets this is also exercising
    /// the demuxer's shard check and the arrival path a real deployment uses.
    pub fn with_shards(mut self, n: usize) -> Self {
        assert!(n > 0, "a simulation node must have at least one shard");
        self.num_shards = n;
        self
    }

    /// Run a plan, asserting nothing and returning what each participant's link did.
    ///
    /// The entry point randomised scenarios use: a generated plan cannot carry hand-written
    /// thresholds, so it has to hand back measurements for the property to judge. Also the only
    /// way to express a claim *across* runs, since anything asserted inside the simulation is a
    /// panic on a turmoil host.
    pub fn run_collecting(self, plan: Vec<Step>) -> HashMap<&'static str, LinkReport> {
        self.run_inner(plan)
    }

    pub fn run(self, plan: Vec<Step>) {
        self.run_inner(plan);
    }

    fn run_inner(self, plan: Vec<Step>) -> HashMap<&'static str, LinkReport> {
        // Determinism. Both must be in force for the whole plan: the clock so dependencies read
        // simulated rather than real time, the RNG so map iteration order and key generation
        // repeat. Seeded per plan and per thread, since `cargo test` gives each plan its own
        // thread and turmoil drives that plan's hosts on it.
        //
        // The guard is dropped at the end of this function rather than at process exit, so the
        // real clock is back in force before the runtime tears down.
        let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
        let _transport_capture = pulsebeam_agent_native::capture_native_transport();
        crate::sim_rand::set_thread_rng(self.rng_seed);
        fastrand::seed(self.rng_seed);
        // Loss, reordering and duplication come from the shaper's own stream, not
        // from turmoil's RNG, so it has to be seeded too or a sweep replays one
        // impairment pattern under every seed.
        pulsebeam_runtime::net::shaper::seed_impairments(self.rng_seed);
        tracing::info!(
            seed = self.rng_seed,
            "simulation plan seed; replay with `./bazel run //:replay -- --seed {}`",
            self.rng_seed
        );

        let sim_duration = plan
            .iter()
            .filter_map(|s| match s {
                Step::Run { duration, .. } | Step::WaitSinceBootstrapRelease { duration, .. } => {
                    Some(*duration)
                }
                Step::ReleaseHeldReconnect { held_for, .. } => Some(*held_for),
                Step::MeasureSourceWindow { duration, .. } => {
                    Some(*duration + Duration::from_secs(5))
                }
                Step::WaitForSourceVideo { timeout, .. } => Some(*timeout + Duration::from_secs(2)),
                Step::BeginHeldReconnect { timeout, .. }
                | Step::WaitForPublication { timeout, .. }
                | Step::WaitForVideo { timeout, .. }
                | Step::WaitForVideoAtResolution { timeout, .. } => Some(*timeout),
                _ => None,
            })
            .sum::<Duration>()
            + Duration::from_secs(60);

        let mut sim = turmoil::Builder::new()
            .ip_version(self.ip_version.clone())
            .simulation_duration(sim_duration)
            .tick_duration(self.tick_duration)
            .min_message_latency(self.link.min_latency)
            .max_message_latency(self.link.max_latency)
            // turmoil's fail_rate partitions the link and clears its in-flight queue; it is not
            // a per-datagram loss percentage. Ordinary lossy profiles use the simulator's
            // datagram dropper below. Explicit Step::Partition still exercises outages.
            .fail_rate(0.0)
            .rng_seed(self.rng_seed)
            .build();

        let subnet = self.subnet.unwrap_or_else(reserve_subnet);
        let server_ip = if self.ip_version == turmoil::IpVersion::V6 {
            subnet_ip_v6(subnet, 1)
        } else {
            subnet_ip(subnet, 1)
        };
        let coordinator_ip = if self.ip_version == turmoil::IpVersion::V6 {
            subnet_ip_v6(subnet, 254)
        } else {
            subnet_ip(subnet, 254)
        };
        let seed = self.rng_seed;
        let tcp_only = self.tcp_only;
        let udp_only = self.udp_only;
        let num_shards = self.num_shards;
        let initial_topics = if self.predeclare_topics {
            initial_topic_registrations(&plan)
        } else {
            BTreeMap::new()
        };

        let (startup_tx, mut startup_rx) = mpsc::channel(1);
        let (server_tx, mut server_rx) = mpsc::channel(1);
        sim.host(server_ip, move || {
            let startup_tx = startup_tx.clone();
            async move {
                let rng = pulsebeam_runtime::rand::seeded_rng(seed);
                start_sfu_node_with_transport(
                    server_ip,
                    rng,
                    num_shards,
                    tcp_only,
                    udp_only,
                    Some(startup_tx),
                )
                .await
                .map_err(Into::into)
            }
        });

        let mut handles = PlanHandles::new();
        let quality_references: Arc<Mutex<BTreeMap<String, &'static QualityVideoReference>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let h264_publishers: Arc<Mutex<FixturePublishers>> = Arc::new(Mutex::new(BTreeMap::new()));
        let mut name_to_ip = PlanIps::new();
        name_to_ip.insert("server", server_ip);
        pulsebeam_runtime::net::shaper::set_gro_window(server_ip, self.link.gro_window);

        // Impairment is keyed by destination, so configuring the SFU's address is what degrades
        // the client-to-SFU direction - the one carrying transport feedback. Leaving it clean
        // tests congestion control with a return path no real network provides.
        if let Some(feedback) = self.link.feedback {
            if let Some(loss) = feedback.loss {
                pulsebeam_runtime::net::shaper::set_loss(server_ip, loss);
            }
            pulsebeam_runtime::net::shaper::set_reorder(server_ip, feedback.reorder);
        }

        let mut ip_counter = 2u8;
        for room in &self.rooms {
            for participant in &room.participants {
                let ip = if self.ip_version == turmoil::IpVersion::V6 {
                    subnet_ip_v6(subnet, ip_counter)
                } else {
                    subnet_ip(subnet, ip_counter)
                };
                ip_counter += 1;

                name_to_ip.insert(participant.name, ip);
                pulsebeam_runtime::net::shaper::set_gro_window(ip, self.link.gro_window);
                // The runtime shaper sits on the SFU's egress socket, so this models loss and
                // bandwidth on the path to the participant. Client UDP sockets bypass it; do not
                // configure a misleading "server" destination here.
                //
                // State is keyed by unique simulation addresses. Do not clear the process-wide
                // registry: cargo runs simulator tests in parallel and clearing it would change
                // the network model of a different test already in flight.
                match self.link.loss_model {
                    Some(model) => pulsebeam_runtime::net::shaper::set_loss(ip, model),
                    None => pulsebeam_runtime::net::shaper::set_packet_loss(ip, self.link.loss),
                }
                pulsebeam_runtime::net::shaper::set_reorder(ip, self.link.reorder);
                pulsebeam_runtime::net::shaper::set_duplicate(ip, self.link.duplicate);

                // Shaping is keyed by destination, so this caps what the SFU can push down to
                // this participant without touching the paths between anyone else.
                if let Some(bps) = self.link.bandwidth_bps {
                    pulsebeam_runtime::net::shaper::set_downlink(ip, bps);
                }

                let shared = Arc::new(ParticipantShared::new(
                    quality_references.clone(),
                    h264_publishers.clone(),
                    participant.solid_picture.clone(),
                    initial_topics
                        .get(participant.name)
                        .cloned()
                        .unwrap_or_default(),
                ));
                let (cmd_tx, cmd_rx) = mpsc::channel::<ParticipantCmd>(16);

                handles.insert(
                    participant.name,
                    ParticipantHandle {
                        shared: shared.clone(),
                        cmd_tx,
                        room_name: room.name,
                        subscribed_at: None,
                        interval_video_baseline: VideoReceiveStats::default(),
                        interval_decoded_video_baseline: BTreeMap::new(),
                        interval_decoded_size_baseline: BTreeMap::new(),
                        native_video_baseline: Default::default(),
                        native_playback_track: None,
                        native_playback_started: false,
                        native_origin_interval: Instant::now(),
                        native_video_stats_timestamp: None,
                        native_video_decoder_frames: None,
                        publishes_video: matches!(
                            participant.role,
                            Role::Publisher | Role::OwnedPublisher
                        ) || participant.subscribes
                            && !participant.rids.is_empty(),
                        publishes_audio: participant.audio_level_dbov.is_some(),
                        quality_video: participant.quality_video,
                        observes_video: participant.slots > 0 || participant.subscribes,
                        auto_subscribe: participant.auto_subscribe,
                        video_capacity: participant.slots,
                        retirement_deadline: None,
                        present: !participant.starts_disconnected,
                    },
                );

                let task = ParticipantTask {
                    ip,
                    server_ip,
                    config: participant.clone(),
                    room_name: room.name,
                    tcp_only,
                    initial_topics: initial_topics
                        .get(participant.name)
                        .cloned()
                        .unwrap_or_default(),
                };

                sim.client(ip, async move {
                    run_participant(task, shared, cmd_rx)
                        .await
                        .map_err(Into::into)
                });
            }
        }

        let reports: Arc<Mutex<HashMap<&'static str, LinkReport>>> = Default::default();
        let reports_inner = reports.clone();
        sim.client(coordinator_ip, async move {
            let mut handles = handles;
            startup_rx
                .recv()
                .await
                .expect("server startup acknowledgment");
            let mut server = ServerControl {
                commands: server_tx,
                startup_notifications: startup_rx,
            };
            execute_plan(
                plan,
                &mut handles,
                &name_to_ip,
                &reports_inner,
                &mut server,
                self.link,
            )
            .await
            .map_err(Into::into)
        });

        let wall_budget = sim_duration * 3 + Duration::from_secs(120);
        let started = Instant::now();
        loop {
            assert!(
                started.elapsed() <= wall_budget,
                "simulation exceeded wall-clock budget"
            );
            while let Ok(command) = server_rx.try_recv() {
                match command {
                    ServerCommand::Stop(stopped) => {
                        sim.crash(server_ip);
                        let _ = stopped.send(());
                    }
                    ServerCommand::Restart => sim.bounce(server_ip),
                }
            }
            if super::native::step(&mut sim).expect("simulation failed") {
                break;
            }
        }

        reports.lock().expect("reports poisoned").clone()
    }
}
