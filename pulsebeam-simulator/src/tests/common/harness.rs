use crate::tests::common::client::{
    AudioReceiveLog, MAX_CONCEALABLE_GAP, SimClientBuilder, VideoReceiveLog, VideoReceiveStats,
};
use crate::tests::common::{reserve_subnet, run_sim_or_timeout, start_sfu_node_with, subnet_ip};
use pulsebeam_agent::SimulcastLayer;
use pulsebeam_agent::media::VbrProfile;
pub use pulsebeam_runtime::net::shaper::{Capacity, Loss, Reorder};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
// Process-wide shimmed clock, not tokio's: turmoil virtualises `tokio::time::Instant` per host,
// so a stamp taken on the coordinator cannot be compared with one taken inside a participant.
// That mismatch reported every time-to-first-frame as ~5s regardless of what happened.
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub enum Role {
    Publisher,
    Subscriber,
    /// Data-channel-only participant: no video tracks added.
    DataOnly,
}

#[derive(Clone, Debug)]
pub struct Participant {
    pub name: &'static str,
    pub role: Role,
    pub rids: Vec<&'static str>,
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
    /// Make the published payload opaque (simulating SFrame/E2EE), forcing the SFU
    /// to forward on the Dependency Descriptor alone.
    pub opaque_payload: bool,
    /// Model a legacy peer that never negotiates the Dependency Descriptor
    /// extension, exercising the marker/deep-inspection fallback for mixed rooms.
    pub marker_only: bool,
}

impl Participant {
    pub fn publisher(name: &'static str, rids: &[&'static str]) -> Self {
        Self {
            name,
            role: Role::Publisher,
            rids: rids.to_vec(),
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
            marker_only: false,
        }
    }

    pub fn single_publisher(name: &'static str) -> Self {
        Self {
            name,
            role: Role::Publisher,
            rids: Vec::new(),
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
            marker_only: false,
        }
    }

    pub fn subscriber(name: &'static str) -> Self {
        Self {
            name,
            role: Role::Subscriber,
            rids: Vec::new(),
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
            marker_only: false,
        }
    }

    /// Subscriber that opens `slots` RecvOnly video tracks.
    pub fn multi_subscriber(name: &'static str, slots: usize) -> Self {
        Self {
            name,
            role: Role::Subscriber,
            rids: Vec::new(),
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
            role: Role::DataOnly,
            rids: Vec::new(),
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
    pub fn with_temporal_dd(mut self, layers: u8) -> Self {
        self.temporal_dd = Some(layers);
        self
    }

    /// Publish an L1T{layers} DD stream whose payload is opaque, simulating
    /// SFrame/E2EE: the SFU cannot inspect the bitstream and must forward on the
    /// Dependency Descriptor alone.
    pub fn with_opaque_dd(mut self, layers: u8) -> Self {
        self.temporal_dd = Some(layers);
        self.opaque_payload = true;
        self
    }

    /// Never negotiate the Dependency Descriptor: a legacy marker/deep-inspection
    /// peer. Used to test mixed rooms (DD on one side, marker-only on the other).
    pub fn marker_only(mut self) -> Self {
        self.marker_only = true;
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
    pub max_missing_parameter_sets: u64,
    pub max_non_contiguous: u64,
}

impl VideoQuality {
    /// Require at least `n` frames. All other constraints default to zero.
    pub fn min_frames(n: u64) -> Self {
        Self {
            min_frames: n,
            max_missing_parameter_sets: 0,
            max_non_contiguous: 0,
        }
    }

    /// Allow up to `n` keyframes without preceding SPS+PPS.
    #[allow(dead_code)]
    pub fn allow_missing_parameter_sets(mut self, n: u64) -> Self {
        self.max_missing_parameter_sets = n;
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

#[allow(dead_code)]
pub enum Step {
    // ── Time ──────────────────────────────────────────────────────────────
    /// Advance simulated time. All participants run during this window.
    /// Also snapshots TX/RX byte counters for subsequent `*BytesInterval` checks.
    Run {
        description: &'static str,
        duration: Duration,
    },
    StallController {
        duration: Duration,
    },
    SendToWrongShard {
        description: &'static str,
        participant: &'static str,
    },
    /// Force the next `count` reaches of a buggify site to fire.
    ///
    /// The probability decides where failures land, not whether any do; this
    /// guarantees the plan exercises the recovery path at every seed.
    ForceFailure {
        description: &'static str,
        site: &'static str,
        count: u32,
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

    // ── Subscriptions ─────────────────────────────────────────────────────
    /// Call driver.set_subscriptions(). Processed on next drive tick.
    SetSubscriptions {
        description: &'static str,
        participant: &'static str,
        subscriptions: Vec<VideoSubscription>,
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
    /// Distinct from `CheckHeardFrom` and `CheckSpeakerRank`, which both carry history: the first
    /// accumulates everyone heard over the whole run, and the second keeps a displaced speaker's
    /// last rank. Neither can say who is being carried *now*, which is the only thing a pin
    /// claims - it does not promise nobody else was ever heard, it promises this one is.
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

    /// Call driver.declare_publish_topic().
    DeclarePublishTopic {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
    },
    /// Call driver.declare_subscribe_topic(). When `scoped_to` is set, the harness
    /// resolves that participant's runtime participant_id automatically.
    DeclareSubscribeTopic {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        scoped_to: Option<&'static str>,
    },
    /// Queue a payload to be sent on the next drive tick.
    PublishData {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        data: &'static [u8],
    },
    DeclareOrderedPublisher {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
    },
    DeclareOrderedSubscriber {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
    },
    PublishOrdered {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        data: &'static [u8],
    },

    // ── Assertions ─────────────────────────────────────────────────────────
    /// Deep QoE check: frames are renderable, SPS/PPS present before keyframes,
    /// no backward timestamp jumps, bounded sequence gaps.
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
    /// Nothing this participant received was thrown away for want of somewhere to put it.
    ///
    /// Silent loss inside the client, which is invisible from both ends: the frames are missing at
    /// the application and present at the wire. Finding 34 such packets once took hand-rolled
    /// probes at every hop, because nothing measured in between.
    CheckMediaRouted {
        description: &'static str,
        participant: &'static str,
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
    CheckRoutingCounter {
        description: &'static str,
        name: &'static str,
        exact: u64,
    },
    CheckRoutingCounterAtLeast {
        description: &'static str,
        name: &'static str,
        min: u64,
    },
    /// Assert a routing counter stops climbing over `over`.
    ///
    /// Steering is a cache, so cross-shard forwarding is expected while a flow
    /// bootstraps and expected to stop once the flow is pinned. A rate that
    /// never reaches zero means the map is not being populated, which no
    /// total-count assertion can distinguish from ordinary bootstrap traffic.
    CheckRoutingCounterSettles {
        description: &'static str,
        name: &'static str,
        over: Duration,
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
    /// Exactly these speakers were heard, and no others.
    ///
    /// The claim the SFU's speaker selection actually makes. A byte count says audio arrived; only
    /// naming the speakers says the right ones did, which is the difference between a selector
    /// that works and one that forwards whoever happens to be first.
    CheckHeardFrom {
        description: &'static str,
        participant: &'static str,
        expected: &'static [&'static str],
    },
    /// A listener is never handed more audio streams than it has slots, and none of them is torn.
    ///
    /// The browser-facing invariant, and the reason the egress SSRC belongs to the slot rather
    /// than the speaker. libwebrtc answers an SSRC it did not see in the SDP by building a whole
    /// new `AudioReceiveStream` - a cold NetEq, four kept per m-line and the oldest destroyed - so
    /// minting one per speaker churns jitter buffers several times a minute to express something a
    /// browser cannot read anyway: it has one `MediaStreamTrack` per transceiver and routes by
    /// mid. Worse, when the SDP *did* declare an SSRC, the receiver binds its sink to that one
    /// specifically and media on any other is decoded and thrown away.
    ///
    /// So slots keep their stream whoever is in it, and who that is travels in the assignment.
    /// What the SFU owes in exchange is a stream that does not tear across the changes: a hole is
    /// loss to the receiver, however deliberate it was.
    CheckAudioStreams {
        description: &'static str,
        participant: &'static str,
        /// How many distinct speakers must have been heard, so the plan cannot pass on silence.
        min_speakers: usize,
        /// How many RTP streams the listener may be sent - its slot count, never more.
        max_streams: usize,
    },
    /// Where the SFU said each speaker sat, loudest first, at their best moment.
    ///
    /// Distinct from `CheckHeardFrom`: that one asserts audio arrived, this one asserts the
    /// listener was *told* who it was and in what order. The two come apart exactly where the
    /// bug did - the media can flow while the assignment carrying the speaker's name does not,
    /// and then no application can attribute a voice to a face.
    CheckSpeakerRank {
        description: &'static str,
        participant: &'static str,
        expected: &'static [(&'static str, u32)],
    },
    /// At least this many media frames actually crossed a shard boundary.
    ///
    /// Guards a cross-shard plan against passing by accident. Placement is a
    /// hash, so a room can land co-located; delivery then looks identical while
    /// reaching none of the route or envelope code the plan exists to cover.
    CheckCrossShardMedia {
        description: &'static str,
        min_frames: u64,
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
    /// At least `min_percent` of what a participant received was actual media.
    ///
    /// Compares the media payload the SFU forwarded against what the subscriber's RTP stats
    /// report receiving. The difference is retransmission and padding generated below the SFU -
    /// so a low ratio means the link is carrying overhead instead of video. Chrome shows the same
    /// quantity as `retransmittedBytesReceived` against `bytesReceived`.
    ///
    /// Only meaningful with a single subscribing participant, since the forwarded counter is
    /// per-simulation rather than per-participant.
    CheckMediaEfficiency {
        description: &'static str,
        participant: &'static str,
        min_percent: u64,
    },
    /// At least `min_quality` is currently being forwarded to subscribers of `origin`'s track.
    ///
    /// `min_quality` uses `LayerQuality`'s numeric rank: 1=Low, 2=Medium, 3=High. Reads the last
    /// allocation pass's decision, so it describes the current steady state rather than a window
    /// - unlike the byte checks, there is no reset between `Step::Run`s.
    ///
    /// Only meaningful when `origin` has exactly one subscriber in the plan; the metric is keyed
    /// by origin because the question this exists to ask - "is this stream regressing" - is about
    /// the publisher's track, not about any one viewer.
    CheckForwardedQuality {
        description: &'static str,
        origin: &'static str,
        min_quality: u8,
    },
    /// Assert that `origin` reached at least `min_quality` at some point during the last
    /// [`Step::Run`]. This is useful for a transition whose final allocation may legitimately
    /// rebalance afterward.
    CheckForwardedQualityReached {
        description: &'static str,
        origin: &'static str,
        min_quality: u8,
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
    },
    /// Configure the loss model for a participant's downlink.
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
    /// The downstream bandwidth estimate on every participant stayed at or above `min_bps`
    /// during the last `Step::Run`.
    ///
    /// Distinct from the byte checks: a poisoned estimate does not necessarily reduce throughput,
    /// because the allocator just drops to a lower simulcast layer and the viewer keeps receiving
    /// something. This asserts the estimate itself.
    CheckMinBwe {
        description: &'static str,
        min_bps: u64,
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
    CheckDataSequence {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        expected: &'static [&'static [u8]],
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

enum PendingDriverOp {
    SetSubscriptions(Vec<VideoSubscription>),
    DeclarePublishTopic(String),
    /// (topic, scoped_participant_id)
    DeclareSubscribeTopic(String, Option<String>),
    PublishData(String, Vec<u8>),
    DeclareOrderedPublisher(String),
    DeclareOrderedSubscriber(String),
    PublishOrdered(String, Vec<u8>),
    /// (pinned participant ids, auto)
    SetAudioIntent(Vec<String>, bool),
}

#[derive(Clone)]
pub struct VideoSubscription {
    pub participant_id: String,
    pub height: u32,
    pub min_height: u32,
    pub priority: u32,
}

// ── Per-participant shared state ────────────────────────────────────────────

struct ParticipantShared {
    video_rx: Arc<Mutex<VideoReceiveLog>>,
    audio_rx: Arc<Mutex<AudioReceiveLog>>,
    paused_publishers: Arc<Mutex<BTreeSet<String>>>,
    tx_bytes: Mutex<u64>,
    rx_bytes: Mutex<u64>,
    connected: Mutex<bool>,
    /// Cumulative keyframe (PLI) requests this participant's publisher received.
    keyframe_requests: Mutex<u64>,
    unroutable_media_dropped: Mutex<u64>,
    media_kinds: Mutex<HashMap<String, (bool, bool)>>,
    /// Every participant id this name has had, oldest first. All but the last are dead identities.
    incarnations: Mutex<Vec<String>>,
    /// Set to Some(...) once the participant has connected for the first time.
    participant_id: Mutex<Option<String>>,
    /// Operations queued by the coordinator; drained on next drive tick.
    pending_ops: Mutex<Vec<PendingDriverOp>>,
    /// Data received per topic, accumulated across all drive ticks.
    data_received: Mutex<HashMap<String, Vec<Vec<u8>>>>,
    /// Remote video tracks discovered via signaling (track IDs).
    discovered_tracks: Mutex<HashSet<String>>,
}

impl ParticipantShared {
    fn new() -> Self {
        Self {
            video_rx: Arc::new(Mutex::new(VideoReceiveLog::default())),
            audio_rx: Arc::new(Mutex::new(AudioReceiveLog::default())),
            paused_publishers: Arc::new(Mutex::new(BTreeSet::new())),
            tx_bytes: Mutex::new(0),
            rx_bytes: Mutex::new(0),
            connected: Mutex::new(false),
            keyframe_requests: Mutex::new(0),
            unroutable_media_dropped: Mutex::new(0),
            media_kinds: Mutex::new(HashMap::new()),
            incarnations: Mutex::new(Vec::new()),
            participant_id: Mutex::new(None),
            pending_ops: Mutex::new(Vec::new()),
            data_received: Mutex::new(HashMap::new()),
            discovered_tracks: Mutex::new(HashSet::new()),
        }
    }
}

struct ParticipantHandle {
    shared: Arc<ParticipantShared>,
    cmd_tx: mpsc::Sender<ParticipantCmd>,
    /// TX bytes at the start of the most recent Step::Run (for interval checks).
    interval_tx_baseline: u64,
    /// RX bytes at the start of the most recent Step::Run (for interval checks).
    interval_rx_baseline: u64,
    /// When this participant last asked for a stream. Time-to-first-frame is measured from here,
    /// not from the start of the plan: the settle steps before a subscription are not time the
    /// viewer spent waiting.
    subscribed_at: Option<Instant>,
    interval_video_baseline: VideoReceiveStats,
    /// Ground truth, from the plan rather than from anything the SFU said.
    ///
    /// The room invariant is checked against these: what this participant actually publishes, and
    /// whether it is currently in the call at all. A client's belief is only interesting next to
    /// something known to be true.
    publishes_video: bool,
    publishes_audio: bool,
    /// Whether the plan has this participant in the room right now.
    present: bool,
    /// How each departure ended, in order: `true` for a graceful leave, `false` for a crash.
    ///
    /// Per departure rather than per participant, because a rejoin makes the previous identity
    /// superseded and how *that* one ended is what decides whether a lingering ghost of it is the
    /// SFU's fault or the network's.
    departures: Vec<bool>,
    /// Whether the last departure was a clean one.
    ///
    /// A graceful leave tells the SFU directly, so everyone should know at once and a ghost is a
    /// bug. A crash is only noticed when the transport times out, which takes seconds and is a
    /// property of the network rather than of state management - so the invariant does not hold
    /// anybody to it.
    departed_cleanly: bool,
}

impl ParticipantHandle {
    fn send_command(&self, command: ParticipantCmd) {
        // Best-effort: a participant that has abruptly exited has dropped its
        // command receiver, so a send can legitimately fail during teardown
        // races. That is not a test failure.
        let _ = self.cmd_tx.try_send(command);
    }

    fn tx_bytes(&self) -> u64 {
        *self.shared.tx_bytes.lock().unwrap()
    }
    fn rx_bytes(&self) -> u64 {
        *self.shared.rx_bytes.lock().unwrap()
    }
    fn keyframe_requests(&self) -> u64 {
        *self.shared.keyframe_requests.lock().unwrap()
    }

    /// What kinds of media this client believes `publisher_id` is sending.
    fn media_kinds_of(&self, publisher_id: &str) -> (bool, bool) {
        *self
            .shared
            .media_kinds
            .lock()
            .unwrap()
            .get(publisher_id)
            .unwrap_or(&(false, false))
    }

    /// Media this participant received and could not hand to anyone. Should always be zero.
    fn unroutable_media_dropped(&self) -> u64 {
        *self.shared.unroutable_media_dropped.lock().unwrap()
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

    fn paused_publishers(&self) -> BTreeSet<String> {
        self.shared.paused_publishers.lock().unwrap().clone()
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
        self.interval_tx_baseline = self.tx_bytes();
        self.interval_rx_baseline = self.rx_bytes();
        self.interval_video_baseline = self.video_rx().stats();
    }
}

// ── Participant task ────────────────────────────────────────────────────────

async fn run_participant(
    ip: IpAddr,
    server_ip: IpAddr,
    config: Participant,
    room_name: &'static str,
    shared: Arc<ParticipantShared>,
    mut cmd_rx: mpsc::Receiver<ParticipantCmd>,
    tcp_only: bool,
) -> anyhow::Result<()> {
    // Participants declared starts_disconnected wait for a Join/Reconnect command.
    if config.starts_disconnected {
        match cmd_rx.recv().await {
            Some(ParticipantCmd::Reconnect) => {}
            _ => return Ok(()),
        }
    }

    loop {
        let mut builder = if tcp_only {
            SimClientBuilder::bind_tcp(ip, server_ip).await?
        } else {
            SimClientBuilder::bind(ip, server_ip).await?
        };

        if config.marker_only {
            builder = builder.without_dependency_descriptor();
        }

        match config.role {
            Role::Publisher => {
                let layers = if config.rids.is_empty() {
                    None
                } else {
                    Some(config.rids.iter().map(|r| SimulcastLayer::new(r)).collect())
                };
                builder = builder.publish_video(layers);
                if let Some(profile) = config.vbr {
                    builder = builder.with_vbr(profile);
                }
                if let Some(layers) = config.temporal_dd {
                    builder = builder.with_temporal_dd(layers);
                }
                if config.opaque_payload {
                    builder = builder.with_opaque_payload();
                }
                if config.subscribes {
                    builder = builder.receive_video(config.slots.max(1));
                }
            }
            Role::Subscriber => {
                builder = builder.receive_video(config.slots.max(1));
            }
            Role::DataOnly => {
                // No tracks; data channels only.
            }
        }

        // After the role's video slots. Transceivers are reserved in order, so putting audio first
        // shifts the mids the video paths are matched on and the viewer receives nothing at all.
        if let Some(level) = config.audio_level_dbov {
            builder = builder.publish_audio(level, config.audio_phase_offset);
        }
        if config.audio_slots > 0 {
            builder = builder.receive_audio(config.audio_slots);
        }

        if !config.auto_subscribe {
            builder = builder.manual_subscriptions();
        }

        let auto_subscribe =
            config.auto_subscribe && (matches!(config.role, Role::Subscriber) || config.subscribes);
        let shared_clone = shared.clone();
        let mut client = builder
            .with_paused_publishers(shared.paused_publishers.clone())
            .with_audio_rx(shared.audio_rx.clone())
            .with_video_rx(shared.video_rx.clone())
            .connect(room_name)
            .await?;

        // A reconnect makes a *new* participant, with a new id, and the plan needs both facts:
        // which identity is live now, and which ones are dead. A client still holding a dead one
        // is a ghost, and that is invisible if the harness only remembers the first.
        {
            let id = client.ctx.agent.participant_id().clone();
            *shared.participant_id.lock().unwrap() = Some(id.clone());
            shared.incarnations.lock().unwrap().push(id);
        }
        *shared.connected.lock().unwrap() = true;

        // Drive until cancelled or a lifecycle command arrives.
        let token = CancellationToken::new();
        let ops_token = token.clone();
        let cmd = {
            let mut drive_fut = Box::pin(client.drive_until_cancelled(token.clone(), move |ctx| {
                // 1. Drain pending ops.
                let ops: Vec<PendingDriverOp> =
                    shared_clone.pending_ops.lock().unwrap().drain(..).collect();
                let mut retry_ops: Vec<PendingDriverOp> = Vec::new();
                for op in ops {
                    match op {
                        PendingDriverOp::SetSubscriptions(subs) => {
                            let incoming_tracks = ctx.incoming_track_tx.clone();
                            // Every subscription is (re-)issued, including ones for a track that
                            // is already subscribed. `SetSubscriptions` is a declarative step, and
                            // a plan's whole point may be to change the *height* of an existing
                            // subscription. Filtering on participant id alone silently dropped
                            // those: a subscriber participant auto-subscribes at 720 as soon as it
                            // discovers a track, so any later `SubscribeTo` naming that track was
                            // a no-op and the plan quietly tested 720p instead of what it asked
                            // for.
                            let new_subscriptions: Vec<_> = subs.to_vec();
                            for subscription in &subs {
                                ctx.requested_tracks
                                    .insert(subscription.participant_id.clone());
                            }
                            for subscription in new_subscriptions {
                                let agent = ctx.agent.clone();
                                let incoming_tracks = incoming_tracks.clone();
                                let token = ops_token.clone();
                                tokio::spawn(async move {
                                    let participant = subscription.participant_id.clone();
                                    // A subscribe that never resolves would otherwise sit here
                                    // until the plan ends and the agent closes, then panic during
                                    // teardown - long after the step that issued it, and pointing
                                    // at shutdown rather than at the subscription that hung.
                                    let result = tokio::select! {
                                        _ = token.cancelled() => {
                                            panic!(
                                                "subscribe to {participant} never resolved; it was \
                                                 still pending when the plan finished"
                                            );
                                        }
                                        result = agent
                                            .participant(subscription.participant_id)
                                            .video()
                                            .subscribe()
                                            .target_height(subscription.height)
                                            .minimum_height(subscription.min_height)
                                            .priority(subscription.priority) => result,
                                    };
                                    let track = result.unwrap_or_else(|e| {
                                        panic!("failed to subscribe to {participant}: {e:?}")
                                    });
                                    if incoming_tracks.send(track).await.is_err() {
                                        // The client is shutting down and no longer reading; the
                                        // subscription itself succeeded, so this is not a failure.
                                    }
                                });
                            }
                        }
                        PendingDriverOp::DeclarePublishTopic(t) => {
                            let agent = ctx.agent.clone();
                            let publishers = ctx.published_topics.clone();
                            tokio::spawn(async move {
                                let publisher = agent
                                    .topic(t.clone())
                                    .expect("invalid topic")
                                    .publisher()
                                    .latest()
                                    .await
                                    .expect("failed to declare publisher");
                                publishers.lock().unwrap().insert(t, publisher);
                            });
                        }
                        PendingDriverOp::DeclareSubscribeTopic(t, pub_id) => {
                            let agent = ctx.agent.clone();
                            let subscribers = ctx.subscribed_topics.clone();
                            tokio::spawn(async move {
                                let builder = agent
                                    .topic(t.clone())
                                    .expect("invalid topic")
                                    .subscriber()
                                    .latest();
                                let builder = match pub_id.as_deref() {
                                    Some(publisher_id) => builder.from_publisher(publisher_id),
                                    None => builder,
                                };
                                let subscriber =
                                    builder.await.expect("failed to declare subscriber");
                                subscribers.lock().unwrap().insert((t, pub_id), subscriber);
                            });
                        }
                        PendingDriverOp::PublishData(ref topic, ref data) => {
                            if let Some(publisher) = ctx.published_topics.lock().unwrap().get(topic)
                            {
                                if publisher.try_send(data.clone()).is_err() {
                                    retry_ops.push(op);
                                }
                            } else {
                                retry_ops.push(op);
                            }
                        }
                        PendingDriverOp::DeclareOrderedPublisher(t) => {
                            let agent = ctx.agent.clone();
                            let publishers = ctx.ordered_publishers.clone();
                            tokio::spawn(async move {
                                let publisher = agent
                                    .topic(t.clone())
                                    .expect("invalid topic")
                                    .publisher()
                                    .ordered()
                                    .await
                                    .expect("failed to declare ordered publisher");
                                publishers.lock().unwrap().insert(t, publisher);
                            });
                        }
                        PendingDriverOp::DeclareOrderedSubscriber(t) => {
                            let agent = ctx.agent.clone();
                            let subscribers = ctx.ordered_subscribers.clone();
                            tokio::spawn(async move {
                                let subscriber = agent
                                    .topic(t.clone())
                                    .expect("invalid topic")
                                    .subscriber()
                                    .ordered()
                                    .await
                                    .expect("failed to declare ordered subscriber");
                                subscribers.lock().unwrap().insert(t, subscriber);
                            });
                        }
                        PendingDriverOp::PublishOrdered(ref topic, ref data) => {
                            if let Some(publisher) =
                                ctx.ordered_publishers.lock().unwrap().get(topic)
                            {
                                if publisher.try_send(data.clone()).is_err() {
                                    retry_ops.push(op);
                                }
                            } else {
                                retry_ops.push(op);
                            }
                        }
                        PendingDriverOp::SetAudioIntent(ref publishers, auto) => {
                            // The wire pins tracks, so each publisher resolves through what this
                            // agent has actually been told it publishes. A pin for somebody whose
                            // audio it has not discovered yet is retried rather than sent empty -
                            // an intent missing its pin would look like a passing plan that
                            // asserted nothing.
                            let mut pinned = Vec::new();
                            let mut unresolved = false;
                            for id in publishers {
                                let tracks = ctx.agent.participant(id.clone()).audio_tracks();
                                if tracks.is_empty() {
                                    unresolved = true;
                                    break;
                                }
                                pinned.extend(tracks);
                            }
                            if unresolved {
                                retry_ops.push(op);
                            } else {
                                let agent = ctx.agent.clone();
                                tokio::spawn(async move {
                                    let _ = agent
                                        .media()
                                        .set_audio_intent(pulsebeam_agent::agent::AudioIntent {
                                            pinned,
                                            auto,
                                        })
                                        .await;
                                });
                            }
                        }
                    }
                }
                if !retry_ops.is_empty() {
                    let mut guard = shared_clone.pending_ops.lock().unwrap();
                    // Prepend so retries are tried first next tick.
                    retry_ops.extend(guard.drain(..));
                    *guard = retry_ops;
                }

                // 1b. Auto-subscribe: for subscriber participants, subscribe to any
                // newly-discovered tracks so tests that have no explicit SubscribeAll
                // step still receive video.
                if auto_subscribe {
                    let incoming_tracks = ctx.incoming_track_tx.clone();
                    let new_track_ids: Vec<String> = ctx
                        .discovered_tracks
                        .iter()
                        .filter(|id| !ctx.requested_tracks.contains(*id))
                        .cloned()
                        .collect();
                    for participant_id in new_track_ids {
                        ctx.requested_tracks.insert(participant_id.clone());
                        let agent = ctx.agent.clone();
                        let incoming_tracks = incoming_tracks.clone();
                        tokio::spawn(async move {
                            // Subscribing can legitimately fail when the publisher
                            // has already left (churn, abrupt exit). That is not a
                            // test failure — just skip it.
                            let Ok(track) = agent
                                .participant(participant_id)
                                .video()
                                .subscribe()
                                .target_height(720)
                                .minimum_height(0)
                                .priority(0)
                                .await
                            else {
                                return;
                            };
                            let _ = incoming_tracks.send(track).await;
                        });
                    }
                }

                // 2. Drain received data from all known subscribers.
                {
                    let mut data_received = shared_clone.data_received.lock().unwrap();
                    for ((topic, _scope), subscriber) in
                        ctx.subscribed_topics.lock().unwrap().iter_mut()
                    {
                        while let Ok(payload) = subscriber.try_recv() {
                            data_received
                                .entry(topic.clone())
                                .or_default()
                                .push(payload);
                        }
                    }
                    for (topic, subscriber) in ctx.ordered_subscribers.lock().unwrap().iter_mut() {
                        while let Ok(delivery) = subscriber.try_recv() {
                            if let pulsebeam_agent::agent::OrderedTopicDelivery::Message(message) =
                                delivery
                            {
                                data_received
                                    .entry(topic.clone())
                                    .or_default()
                                    .push(message.payload);
                            }
                        }
                    }
                }

                // 3. Snapshot discovered tracks, and what kind of media each is believed to have.
                {
                    *shared_clone.discovered_tracks.lock().unwrap() = ctx.discovered_tracks.clone();
                    let mut kinds = shared_clone.media_kinds.lock().unwrap();
                    kinds.clear();
                    for id in &ctx.discovered_tracks {
                        let participant = ctx.agent.participant(id.clone());
                        kinds.insert(
                            id.clone(),
                            (participant.has_video(), participant.has_audio()),
                        );
                    }
                }

                // 4. Update stats.
                let stats = ctx.agent.stats().current();
                *shared_clone.tx_bytes.lock().unwrap() = stats.total_tx_bytes();
                *shared_clone.rx_bytes.lock().unwrap() = stats.total_rx_bytes();
                *shared_clone.connected.lock().unwrap() = stats.is_connected();
                *shared_clone.keyframe_requests.lock().unwrap() =
                    stats.keyframe_requests_received();
                *shared_clone.unroutable_media_dropped.lock().unwrap() =
                    stats.unroutable_media_dropped();
                false
            }));

            let received = tokio::select! {
                _ = drive_fut.as_mut() => None,
                c = cmd_rx.recv() => { token.cancel(); c }
            };
            drop(drive_fut); // release &mut client borrow
            received
        };

        // Update stats one final time before handling the command.
        {
            let stats = client.ctx.agent.stats().current();
            *shared.tx_bytes.lock().unwrap() = stats.total_tx_bytes();
            *shared.rx_bytes.lock().unwrap() = stats.total_rx_bytes();
        }
        *shared.connected.lock().unwrap() = false;

        match cmd {
            None | Some(ParticipantCmd::Done) => break,
            Some(ParticipantCmd::Shutdown) => {
                client.ctx.agent.close().await?;
                drop(client);
                match cmd_rx.recv().await {
                    Some(ParticipantCmd::Reconnect) => continue,
                    _ => break,
                }
            }
            Some(ParticipantCmd::Drop) => {
                drop(client);
                match cmd_rx.recv().await {
                    Some(ParticipantCmd::Reconnect) => continue,
                    _ => break,
                }
            }
            Some(ParticipantCmd::Reconnect) => continue,
        }
    }

    Ok(())
}

// ── Coordinator task ────────────────────────────────────────────────────────

fn step_name(step: &Step) -> &'static str {
    match step {
        Step::Run { .. } => "Run",
        Step::StallController { .. } => "StallController",
        Step::SendToWrongShard { .. } => "SendToWrongShard",
        Step::FailNextMaterialization { .. } => "FailNextMaterialization",
        Step::ForceFailure { .. } => "ForceFailure",
        Step::Partition { .. } => "Partition",
        Step::Repair { .. } => "Repair",
        Step::Hold { .. } => "Hold",
        Step::Release { .. } => "Release",
        Step::Join { .. } => "Join",
        Step::Disconnect { .. } => "Disconnect",
        Step::AbruptExit { .. } => "AbruptExit",
        Step::Reconnect { .. } => "Reconnect",
        Step::SetSubscriptions { .. } => "SetSubscriptions",
        Step::SubscribeAll { .. } => "SubscribeAll",
        Step::SubscribeTo { .. } => "SubscribeTo",
        Step::SubscribeToQos { .. } => "SubscribeToQos",
        Step::DeclarePublishTopic { .. } => "DeclarePublishTopic",
        Step::DeclareSubscribeTopic { .. } => "DeclareSubscribeTopic",
        Step::PublishData { .. } => "PublishData",
        Step::DeclareOrderedPublisher { .. } => "DeclareOrderedPublisher",
        Step::DeclareOrderedSubscriber { .. } => "DeclareOrderedSubscriber",
        Step::PublishOrdered { .. } => "PublishOrdered",
        Step::CheckVideoQuality { .. } => "CheckVideoQuality",
        Step::CheckVideoQualityInterval { .. } => "CheckVideoQualityInterval",
        Step::CheckKeyframeRequests { .. } => "CheckKeyframeRequests",
        Step::CheckKeyframeRequestsAtLeast { .. } => "CheckKeyframeRequestsAtLeast",
        Step::CheckRoutingCounter { .. } => "CheckRoutingCounter",
        Step::CheckRoutingCounterAtLeast { .. } => "CheckRoutingCounterAtLeast",
        Step::CheckRoutingCounterSettles { .. } => "CheckRoutingCounterSettles",
        Step::CheckMediaRouted { .. } => "CheckMediaRouted",
        Step::CheckParticipantsKnown { .. } => "CheckParticipantsKnown",
        Step::CheckIdentityStable { .. } => "CheckIdentityStable",
        Step::CheckConnected { .. } => "CheckConnected",
        Step::CheckNotConnected { .. } => "CheckNotConnected",
        Step::CheckRxBytes { .. } => "CheckRxBytes",
        Step::CheckHeardFrom { .. } => "CheckHeardFrom",
        Step::CheckSpeakerRank { .. } => "CheckSpeakerRank",
        Step::CheckAudioStreams { .. } => "CheckAudioStreams",
        Step::CheckCrossShardMedia { .. } => "CheckCrossShardMedia",
        Step::CheckTxBytes { .. } => "CheckTxBytes",
        Step::CheckRxBytesInterval { .. } => "CheckRxBytesInterval",
        Step::CheckMaxRxBytesInterval { .. } => "CheckMaxRxBytesInterval",
        Step::SetBandwidth { .. } => "SetBandwidth",
        Step::SetCapacity { .. } => "SetCapacity",
        Step::SetLoss { .. } => "SetLoss",
        Step::SetReorder { .. } => "SetReorder",
        Step::Expect { .. } => "Expect",
        Step::Report { .. } => "Report",
        Step::CheckMediaEfficiency { .. } => "CheckMediaEfficiency",
        Step::CheckForwardedQuality { .. } => "CheckForwardedQuality",
        Step::CheckForwardedQualityReached { .. } => "CheckForwardedQualityReached",
        Step::CheckTxBytesInterval { .. } => "CheckTxBytesInterval",
        Step::CheckMinBwe { .. } => "CheckMinBwe",
        Step::CheckDataReceived { .. } => "CheckDataReceived",
        Step::CheckDataNotReceived { .. } => "CheckDataNotReceived",
        Step::CheckDataSequence { .. } => "CheckDataSequence",
        Step::SetAudioIntent { .. } => "SetAudioIntent",
        Step::CheckSpeakerHeld { .. } => "CheckSpeakerHeld",
    }
}

type PlanHandles = BTreeMap<&'static str, ParticipantHandle>;
type PlanIps = BTreeMap<&'static str, IpAddr>;

async fn execute_plan(
    plan: Vec<Step>,
    handles: &mut PlanHandles,
    name_to_ip: &PlanIps,
    reports: &Mutex<HashMap<&'static str, LinkReport>>,
) -> anyhow::Result<()> {
    let total = plan.len();
    // Duration of the most recent Run, so interval properties know what window they describe.
    let mut window = Duration::ZERO;

    for (idx, step) in plan.iter().enumerate() {
        let n = idx + 1;
        let kind = step_name(step);

        match step {
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
                }
            }

            Step::StallController { duration } => {
                tracing::info!("[step {n}/{total}: {kind}] controller stalled for {duration:?}");
                pulsebeam::sim_metrics::request_controller_stall(*duration);
                tokio::time::sleep(*duration).await;
            }

            Step::SendToWrongShard {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let _ = get_handle(handles, participant, description)?;
                let source = resolve(name_to_ip, participant, description)?;
                pulsebeam_runtime::net::set_wrong_owner_injection(source);
                handles
                    .get(participant)
                    .expect("participant was resolved above")
                    .send_command(ParticipantCmd::Reconnect);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            Step::ForceFailure {
                description,
                site,
                count,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({site} x{count})");
                pulsebeam_runtime::buggify::force(site, *count);
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
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, {capacity:?})"
                );
                let ip = resolve(name_to_ip, participant, description)?;
                pulsebeam_runtime::net::shaper::set_capacity(
                    ip,
                    *capacity,
                    Duration::from_millis(200),
                );
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
                if let Err(reason) = check_property(property, handle, ip, window, handles) {
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
                turmoil::release(from_ip, to_ip);
            }

            Step::Join {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                // Ground truth for the room invariant, from the plan rather than the wire.
                handle.present = true;
                handle.departed_cleanly = true;
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
                handle.departed_cleanly = true;
                handle.departures.push(true);
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
                handle.departed_cleanly = false;
                handle.departures.push(false);
                handle.send_command(ParticipantCmd::Drop);
            }

            Step::Reconnect {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                // Ground truth for the room invariant, from the plan rather than the wire.
                handle.present = true;
                handle.departed_cleanly = true;
                handle.send_command(ParticipantCmd::Reconnect);
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
                        height: *height,
                        min_height: 0,
                        priority: 0,
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
                        height: *height,
                        min_height: *min_height,
                        priority: *priority,
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
                        height: heights_slice[i % heights_slice.len()],
                        min_height: 0,
                        priority: 0,
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
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, topic={topic})"
                );
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::DeclarePublishTopic(topic.to_string()));
            }

            Step::DeclareSubscribeTopic {
                description,
                participant,
                topic,
                scoped_to,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, topic={topic}, scoped_to={scoped_to:?})"
                );
                let resolved_id: Option<String> = match scoped_to {
                    None => None,
                    Some(pub_name) => {
                        let pub_handle = handles.get(*pub_name).ok_or_else(|| {
                            anyhow::anyhow!("step \"{description}\": unknown publisher participant \"{pub_name}\"")
                        })?;
                        let id = pub_handle.shared.participant_id.lock().unwrap().clone();
                        match id {
                            Some(id) => Some(id),
                            None => anyhow::bail!(
                                "step \"{description}\": DeclareSubscribeTopic scoped_to \"{pub_name}\" \
                                 but that participant has not connected yet; add a Step::Run before this step"
                            ),
                        }
                    }
                };
                let handle = get_handle(handles, participant, description)?;
                handle.shared.pending_ops.lock().unwrap().push(
                    PendingDriverOp::DeclareSubscribeTopic(topic.to_string(), resolved_id),
                );
            }

            Step::PublishData {
                description,
                participant,
                topic,
                data,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, topic={topic}, {} bytes)",
                    data.len()
                );
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::PublishData(
                        topic.to_string(),
                        data.to_vec(),
                    ));
            }

            Step::DeclareOrderedPublisher {
                description,
                participant,
                topic,
            } => {
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::DeclareOrderedPublisher(topic.to_string()));
            }

            Step::DeclareOrderedSubscriber {
                description,
                participant,
                topic,
            } => {
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::DeclareOrderedSubscriber(topic.to_string()));
            }

            Step::PublishOrdered {
                description,
                participant,
                topic,
                data,
            } => {
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::PublishOrdered(
                        topic.to_string(),
                        data.to_vec(),
                    ));
            }

            Step::CheckVideoQuality {
                description,
                participant,
                quality,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                let log = handle.video_rx();
                assert_video_quality(n, total, description, participant, quality, &log);
            }

            Step::CheckVideoQualityInterval {
                description,
                participant,
                quality,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                let stats = handle.video_stats_since_interval();
                // Only the frame floor. The decodability and continuity bounds are cumulative
                // counters, and a difference between two of them does not mean what it looks
                // like; asking about frames arriving in a window is what this exists for.
                assert!(
                    stats.frames >= quality.min_frames,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {} frames in the last interval\n  actual:       frames={}, keyframes={}\n  note:         a cumulative count would still show every frame from before the\n                disturbance; this asks whether anything arrived after it",
                    quality.min_frames,
                    stats.frames,
                    stats.keyframes,
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
                let ranked = handle.audio_rx().ranked();
                assert!(
                    ranked.contains_key(&want),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     {speaker} to hold a slot\n  signalled:    {ranked:?}"
                );
            }

            Step::CheckMediaRouted {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                let dropped = handle.unroutable_media_dropped();
                assert_eq!(
                    dropped, 0,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     no media discarded for want of a slot to put it in\n  actual:       {dropped} packets\n  note:         these arrived, were decrypted and demuxed, and then went\n                nowhere - invisible at both ends"
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

            Step::CheckRoutingCounter {
                description,
                name,
                exact,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({name})");
                let actual = pulsebeam::sim_metrics::routing_counter(name);
                assert_eq!(
                    actual, *exact,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  counter:     {name}\n  expected:     exactly {exact}\n  actual:       {actual}"
                );
            }

            Step::CheckRoutingCounterAtLeast {
                description,
                name,
                min,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({name})");
                let actual = pulsebeam::sim_metrics::routing_counter(name);
                assert!(
                    actual >= *min,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  counter:     {name}\n  expected:     at least {min}\n  actual:       {actual}"
                );
            }

            Step::CheckRoutingCounterSettles {
                description,
                name,
                over,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({name}, {over:?})");
                let before = pulsebeam::sim_metrics::routing_counter(name);
                tokio::time::sleep(*over).await;
                let after = pulsebeam::sim_metrics::routing_counter(name);
                assert_eq!(
                    before,
                    after,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  counter:     {name}\n  expected:     no change over {over:?}\n  actual:       climbed by {}",
                    after.saturating_sub(before)
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

            Step::CheckHeardFrom {
                description,
                participant,
                expected,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, expected {expected:?})"
                );
                let handle = get_handle(handles, participant, description)?;
                let audio = handle.audio_rx();
                let heard = audio.heard_from();
                let want: BTreeSet<String> =
                    expected.iter().map(|name| (*name).to_owned()).collect();
                // Names in the plan are harness names; the wire carries participant ids, so
                // compare on the ids those names resolve to.
                let want: BTreeSet<String> = want
                    .iter()
                    .filter_map(|name| {
                        handles
                            .get(name.as_str())
                            .and_then(|h| h.shared.participant_id.lock().unwrap().clone())
                    })
                    .collect();
                assert_eq!(
                    heard, want,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     {expected:?}\n  heard from:   {heard:?}\n  per speaker:  {:?}",
                    audio.by_publisher
                );
            }

            Step::CheckAudioStreams {
                description,
                participant,
                min_speakers,
                max_streams,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, ≥{min_speakers} speakers, ≤{max_streams} streams)"
                );
                let handle = get_handle(handles, participant, description)?;
                let audio = handle.audio_rx();
                let heard = audio.heard_from();
                assert!(
                    heard.len() >= *min_speakers,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  expected:     ≥ {min_speakers} distinct speakers heard\n  actual:       {}\n  per speaker:  {:?}\n  note:         fewer speakers than slots means no slot was ever stolen, so\n                the plan proved nothing",
                    heard.len(),
                    audio.by_publisher
                );
                assert!(
                    audio.by_stream.len() <= *max_streams,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  expected:     at most {max_streams} RTP streams, one per slot\n  actual:       {}\n  streams:      {:?}\n  note:         a stream per speaker costs a browser a fresh receive stream and\n                a cold jitter buffer every time a slot changes hands",
                    audio.by_stream.len(),
                    audio.by_stream.keys().collect::<Vec<_>>()
                );
                for (ssrc, stream) in &audio.by_stream {
                    assert!(
                        stream.max_seq_gap <= MAX_CONCEALABLE_GAP,
                        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  stream:       {ssrc}\n  expected:     at most {MAX_CONCEALABLE_GAP} packets missing\n  actual:       a gap of {} packets\n  note:         this plan configures no loss, so a hole in a slot's stream is the\n                SFU splicing two speakers onto it badly",
                        stream.max_seq_gap
                    );
                }
            }

            Step::CheckSpeakerRank {
                description,
                participant,
                expected,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, expected {expected:?})"
                );
                let handle = get_handle(handles, participant, description)?;
                let ranked = handle.audio_rx().ranked();
                let want: std::collections::BTreeMap<String, u32> = expected
                    .iter()
                    .filter_map(|(name, rank)| {
                        handles
                            .get(name)
                            .and_then(|h| h.shared.participant_id.lock().unwrap().clone())
                            .map(|id| (id, *rank))
                    })
                    .collect();
                assert_eq!(
                    ranked, want,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     {expected:?}\n  signalled:    {ranked:?}"
                );
            }

            Step::CheckCrossShardMedia {
                description,
                min_frames,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" (min {min_frames} frames)"
                );
                let actual = pulsebeam::sim_metrics::cross_shard_media_frames();
                assert!(
                    actual >= *min_frames,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  expected:     ≥ {min_frames} frames resolved from another shard\n  actual:       {actual}\n  note:         zero means the room was co-located, so no cross-shard\n                path ran at all — the plan proved nothing"
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

            Step::CheckMinBwe {
                description,
                min_bps,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" (min {min_bps} bps)");
                let observed = pulsebeam::sim_metrics::downstream_bwe_summary();
                let Some((min_seen, max_seen, last_seen, count)) = observed else {
                    panic!(
                        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  expected:     >= {min_bps} bps\n  actual:       no allocation passes observed - the check would pass vacuously"
                    );
                };
                assert!(
                    min_seen >= *min_bps,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  expected:     >= {min_bps} bps on every participant\n  actual:       min {min_seen} / max {max_seen} / last {last_seen} bps over {count} allocation passes"
                );
            }
            Step::CheckMediaEfficiency {
                description,
                participant,
                min_percent,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, min {min_percent}%)"
                );
                let handle = get_handle(handles, participant, description)?;
                let received = handle.rx_bytes();
                let forwarded = handle.forwarded_media();
                assert!(
                    received > 0,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  nothing was received, so the ratio would be meaningless"
                );
                let percent = forwarded.saturating_mul(100) / received;
                assert!(
                    percent >= *min_percent,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     >= {min_percent}% of received bytes to be media\n  actual:       {percent}% ({forwarded} media forwarded / {received} received)"
                );
            }

            Step::CheckForwardedQuality {
                description,
                origin,
                min_quality,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({origin}, min quality {min_quality})"
                );
                let handle = get_handle(handles, origin, description)?;
                let id = handle.shared.participant_id.lock().unwrap().clone();
                let id = id.ok_or_else(|| {
                    anyhow::anyhow!(
                        "step \"{description}\": {origin} has no runtime participant id yet; add a Step::Run before this step"
                    )
                })?;
                let actual = pulsebeam::sim_metrics::forwarded_quality(&id);
                let bwe = pulsebeam::sim_metrics::downstream_bwe_summary();
                assert!(
                    actual.is_some_and(|q| q >= *min_quality),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  origin:       {origin}\n  expected:     >= quality {min_quality}\n  actual:       {actual:?} (0=paused, None=never observed)\n  BWE window:   {bwe:?} (min, max, last, samples)"
                );
            }

            Step::CheckForwardedQualityReached {
                description,
                origin,
                min_quality,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({origin}, min quality {min_quality})"
                );
                let handle = get_handle(handles, origin, description)?;
                let id = handle.shared.participant_id.lock().unwrap().clone();
                let id = id.ok_or_else(|| {
                    anyhow::anyhow!(
                        "step \"{description}\": {origin} has no runtime participant id yet; add a Step::Run before this step"
                    )
                })?;
                let actual = pulsebeam::sim_metrics::max_forwarded_quality(&id);
                assert!(
                    actual.is_some_and(|q| q >= *min_quality),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  origin:       {origin}\n  expected:     reached >= quality {min_quality} during the last Run\n  actual:       {actual:?} (0=paused, None=never observed)"
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
                let baseline = handle.interval_rx_baseline;
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
                let baseline = handle.interval_rx_baseline;
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
                let baseline = handle.interval_tx_baseline;
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
                let data_received = handle.shared.data_received.lock().unwrap();
                let received = data_received
                    .get(*topic)
                    .map(std::vec::Vec::as_slice)
                    .unwrap_or(&[]);
                let expected_vec = expected.to_vec();
                assert!(
                    received.contains(&expected_vec),
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
                let data_received = handle.shared.data_received.lock().unwrap();
                let received = data_received
                    .get(*topic)
                    .map(std::vec::Vec::as_slice)
                    .unwrap_or(&[]);
                let excluded_vec = excluded.to_vec();
                assert!(
                    !received.contains(&excluded_vec),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  topic:        {topic}\n  expected:     payload {excluded:?} NOT in received list\n  actual:       {received:?}"
                );
            }

            Step::CheckDataSequence {
                description,
                participant,
                topic,
                expected,
            } => {
                let handle = get_handle(handles, participant, description)?;
                let data_received = handle.shared.data_received.lock().unwrap();
                let received = data_received
                    .get(*topic)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let expected: Vec<Vec<u8>> =
                    expected.iter().map(|payload| payload.to_vec()).collect();
                assert_eq!(
                    received, expected,
                    "step {n}/{total} {kind}: {description} ({participant}, {topic})"
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

/// What every client must believe about the room, checked against what the plan actually did.
///
/// Run after every `Step::Run` of every plan, for every participant, rather than opted into by the
/// plans that happen to think of it. State-management bugs do not announce themselves: a ghost
/// participant, or somebody counted twice, looks exactly like a passing test to any assertion
/// aimed at bitrate or frames. The two failures this exists for both shipped and were found by
/// hand.
///
/// Deliberately only the *safety* half. "Everyone who should be known is known" is liveness and
/// depends on discovery timing, which would make this flake; what it asserts is that nothing is
/// known that should not be, and that whatever is known is described correctly.
fn assert_room_state_consistent(handles: &PlanHandles, after: &str) {
    // Every participant id the plan has ever created, and what it means now.
    #[derive(Clone, Copy)]
    enum Identity {
        /// The participant's current id, and they are in the room.
        Live,
        /// The current id of somebody the plan has removed.
        Departed,
        /// An id from before a reconnect. That person exists, but not under this name any more.
        Superseded,
        /// Gone without saying so. Nothing is asserted: detection is a timeout, not a message.
        Vanished,
    }
    let mut identities: HashMap<String, (&'static str, Identity)> = HashMap::new();
    for (name, handle) in handles {
        let incarnations = handle.shared.incarnations.lock().unwrap().clone();
        let last = incarnations.len().saturating_sub(1);
        for (i, id) in incarnations.into_iter().enumerate() {
            let state = if i < last {
                // Superseded, but only strictly if that incarnation ended by saying so. One that
                // crashed is found out by timeout like any other.
                if handle.departures.get(i).copied().unwrap_or(true) {
                    Identity::Superseded
                } else {
                    Identity::Vanished
                }
            } else if handle.present {
                Identity::Live
            } else if handle.departed_cleanly {
                Identity::Departed
            } else {
                // Crashed. The SFU only finds out when the transport times out, so how long a
                // ghost lingers is a fact about the network. Not this invariant's business.
                Identity::Vanished
            };
            identities.insert(id, (*name, state));
        }
    }

    for (observer, handle) in handles {
        // Only ask this of clients that are still running. A participant who has left stopped
        // updating its view the moment it went, so its last snapshot is stale by construction -
        // holding it to the room's current membership would report a ghost on every plan that
        // ends by disconnecting everybody.
        if !handle.present {
            continue;
        }
        let known = handle.shared.discovered_tracks.lock().unwrap().clone();
        for id in &known {
            let Some((name, state)) = identities.get(id).copied() else {
                panic!(
                    "\nroom state inconsistent after {after}\n  observer:     {observer}\n  knows:        {id}\n  problem:      no participant in this plan has ever had that id\n  note:         a client holding an id nobody owns is state invented from\n                nowhere - a tile for somebody who never existed"
                );
            };
            match state {
                // Live is fine; Vanished is a crash, whose detection is a timeout rather than a
                // message, so how long the ghost lingers is a fact about the network.
                Identity::Live | Identity::Vanished => {}
                Identity::Departed => panic!(
                    "\nroom state inconsistent after {after}\n  observer:     {observer}\n  knows:        {name} ({id})\n  problem:      {name} has left the room\n  note:         a ghost - a tile, a name and a publication for somebody who\n                is not in the call"
                ),
                Identity::Superseded => panic!(
                    "\nroom state inconsistent after {after}\n  observer:     {observer}\n  knows:        {id}\n  problem:      that is {name}'s identity from before they reconnected\n  note:         a ghost of an earlier incarnation - {name} is in the room, but\n                under a new id, so this is a second tile for one person"
                ),
            }
            let Some(subject) = handles.get(name) else {
                continue;
            };
            let (video, _audio) = handle.media_kinds_of(id);
            assert_eq!(
                video, subject.publishes_video,
                "\nroom state inconsistent after {after}\n  observer:     {observer}\n  subject:      {name}\n  believes video: {video}, actually publishes video: {}\n  note:         a participant believed to send video that does not is a phantom\n                tile; announcing audio in `tracks_upsert` caused exactly this,\n                and put anyone sending both on screen twice",
                subject.publishes_video
            );
        }
    }
}

/// A `Step::Run` shorter than this is a pause mid-transition, not a settled room, so the state
/// invariant is not asked of it. Signalling a join or a departure is a round trip or two.
const ROOM_SETTLE_FLOOR: Duration = Duration::from_secs(1);

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
        log.frames >= quality.min_frames,
        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {} frames\n  actual:       frames={}, keyframes={}, missing_parameter_sets={}, ts_regression_count={}, non_contiguous={}",
        quality.min_frames,
        log.frames,
        log.keyframes,
        log.missing_parameter_sets,
        log.ts_regression_count,
        log.non_contiguous,
    );
    assert!(
        log.missing_parameter_sets <= quality.max_missing_parameter_sets,
        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≤ {} keyframes missing parameter sets (decodable)\n  actual:       frames={}, keyframes={}, missing_parameter_sets={}, ts_regression_count={}, non_contiguous={}",
        quality.max_missing_parameter_sets,
        log.frames,
        log.keyframes,
        log.missing_parameter_sets,
        log.ts_regression_count,
        log.non_contiguous,
    );
    assert!(
        log.non_contiguous <= quality.max_non_contiguous,
        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≤ {} non-contiguous frames (gap budget)\n  actual:       frames={}, keyframes={}, missing_parameter_sets={}, ts_regression_count={}, non_contiguous={}",
        quality.max_non_contiguous,
        log.frames,
        log.keyframes,
        log.missing_parameter_sets,
        log.ts_regression_count,
        log.non_contiguous,
    );
}

// ── LocalNodeSim ────────────────────────────────────────────────────────────

/// A high-level claim about behaviour, stated against ground truth.
///
/// Byte-count assertions (`min_bytes: 3_000_000`) fold link rate, codec, fixture length and
/// current behaviour into one number. They go stale silently — shrinking the screenshare fixture
/// in `8263602` invalidated a batch of them and nothing failed — and being one-sided floors they
/// cannot express collapse, overuse, or instability at all.
///
/// These are stated against what the simulator *configured*, so they survive changes to the
/// fixture, the codec, or the ladder, and they fail when behaviour actually regresses.
///
/// The properties that reference capacity require a `Fixed` link: on a ramp or an oscillation
/// "the capacity" is not a single number, and silently comparing against its instantaneous value
/// would be exactly the kind of quietly-wrong assertion this type exists to remove. Use
/// [`Property::EstimateStable`] and [`Property::QueueingDelayBelow`] on scheduled links.
#[derive(Clone, Copy, Debug)]
// A vocabulary of assertions a plan may make, not a set of call sites. Each
// variant is implemented and documented with the regime it is fair in; a
// variant no current plan happens to use is a claim available to the next one,
// which is the opposite of dead code. Deleting the unused ones would leave the
// suite able to express only what it already asserts.
#[allow(dead_code)]
pub enum Property {
    /// Video frames decoded during the last run after a complete, parameterized keyframe.
    VideoDecodes,
    /// Delivered throughput was at least this percent of the link's capacity.
    ///
    /// Only meaningful when demand meets or exceeds capacity; a plan whose sources cannot fill
    /// the link will fail this for reasons that are not the controller's fault.
    UtilisationAtLeast(u8),
    /// The estimate ended within `within_percent` of true capacity, in *both* directions.
    ///
    /// Two-sided on purpose: an estimate far above capacity is a controller that will overshoot
    /// and drive loss, which a floor-style assertion silently permits.
    ///
    /// Only fair under saturation. When the application asks for less than the link can carry,
    /// a delay-based estimator correctly settles near demand — it cannot measure bandwidth it is
    /// not using. Use [`Property::EstimateMeetsNeed`] unless the plan deliberately overloads the
    /// link.
    EstimateTracksCapacity { within_percent: u8 },
    /// The estimate reached `percent` of whatever is smaller: what the allocator asked for, or
    /// what the link can actually carry.
    ///
    /// The claim that holds in every regime, and the one that matches what a user notices. Below
    /// saturation it means "found enough for what was wanted"; above it, "saturated the link
    /// trying". It cannot be satisfied by an estimate that quietly gave up, and unlike a byte
    /// floor it does not have to be re-tuned when the ladder or the fixture changes.
    EstimateMeetsNeed { percent: u8 },
    /// The estimate never fell more than `max_drop_percent` below its running maximum.
    ///
    /// The collapse detector, and the most valuable property here: it states the production
    /// failure — stable link, estimate falls away and does not recover — directly.
    EstimateStable { max_drop_percent: u8 },
    /// The estimate reached `percent` of capacity within `within` of the window starting.
    EstimateConverges { percent: u8, within: Duration },
    /// The estimate ended within `of_peak_percent` of the highest value it reached.
    ///
    /// A dip is not by itself a defect. A bursty source moves the estimate around, and a
    /// controller that never dips is one that is not responding to anything. What distinguishes
    /// working from broken is whether it comes *back*: the production failure was not that the
    /// estimate fell, it was that it fell and stayed there while the link was fine.
    ///
    /// Prefer this to [`Property::EstimateStable`] wherever the source is bursty. Bounding peak
    /// drawdown there asserts something stronger than the system owes - and, being tuned to
    /// whatever the current dip happens to be, is the kind of threshold that gets quietly relaxed
    /// until it means nothing.
    EstimateRecovers { of_peak_percent: u8 },
    /// Standing queue at the bottleneck stayed below this.
    ///
    /// Bufferbloat: a controller can hold a link perfectly full while parking hundreds of
    /// milliseconds of queue. That looks healthy in throughput and is unusable for a call.
    ///
    /// The *standing* queue, not the deepest moment. A peak is one sample, and every link that
    /// ever filled has a high one - so bounding it fails a controller that dipped into the buffer
    /// once on the way to behaving well, and passes one that sits at 90ms forever. Those are
    /// opposite verdicts on the thing this property exists to catch. Use
    /// [`Property::PeakQueueingDelayBelow`] where the transient genuinely is the claim.
    QueueingDelayBelow(Duration),
    /// The deepest queue occupancy reached, at any instant.
    ///
    /// A strictly stronger and much noisier claim than [`Property::QueueingDelayBelow`]: fair
    /// only where a plan holds capacity steady, since a controller cannot shed rate before
    /// observing a capacity it has not yet been told about.
    PeakQueueingDelayBelow(Duration),
    /// At most this percent of offered packets were dropped by a full bottleneck buffer.
    ///
    /// Congestion drops only, distinct from configured link loss: this is the controller
    /// overusing the link.
    CongestionLossBelow(u8),
    /// A publisher's forwarded layer changed at most this many times per minute.
    ///
    /// The instability check, and the one the rest of this vocabulary cannot express. Every other
    /// property here is an average or an endpoint, and a stream flipping between two layers many
    /// times a second satisfies all of them: the final layer is right, the byte count is right,
    /// the estimate is right. Only the *count of changes* shows it.
    ///
    /// It matters because switching is not free. Each change needs a keyframe and stutters the
    /// picture, so a stream that never holds a layer long enough to decode a run of frames shows
    /// the viewer nothing at all. That is how it presented in production - "the screen share
    /// never appeared" - rather than as visibly poor quality.
    ///
    /// Named per minute because that is the scale a human judges it on. A settled stream changes
    /// a handful of times a minute as conditions move; the production failure was doing it
    /// several times a second.
    QualityChangesPerMinuteBelow { origin: &'static str, max: u64 },
    /// At most this many direction reversals in the origin's forwarded layer.
    ///
    /// Climbing through layers as bandwidth is discovered is correct, so a raw change count cannot
    /// separate a healthy ramp from a stream oscillating between two layers - it fails both. A
    /// reversal is the part that is never right, which makes this assertable across the whole run
    /// including the cold-start ramp, where a joining viewer notices flapping most.
    ///
    /// A monotonic q -> h -> f climb reports zero however many layers it passes through.
    QualityReversalsBelow { origin: &'static str, max: u64 },
    /// The origin's forwarded layer never rose above this rank at any point in the window.
    ///
    /// `target_height` is a ceiling as much as a request: a viewer showing a 180p tile is asking
    /// not to be sent 720p, and sending it anyway wastes the link on pixels nobody will see. Every
    /// other layer assertion here is a floor, and the byte-rate ceiling that stood in for this one
    /// cannot tell a stream forwarded at the wrong rung from one forwarded at the right rung with
    /// a busier picture. This reads the highest rank actually forwarded.
    NeverForwardedAbove {
        origin: &'static str,
        max_quality: u8,
    },
    /// At least this percent of the bytes the viewer received were media payload.
    ///
    /// Measured as media forwarded by the SFU over bytes received by the viewer, which is only a
    /// true efficiency ratio on a lossless path: packets dropped in flight shrink the denominator
    /// without shrinking the numerator, so it can exceed 100% (measured at 103.5% on a link
    /// dropping 7%). Use it to catch overhead — RTX and padding crowding out video — on links
    /// that are not dropping, and disregard it where congestion loss is significant.
    ///
    /// A loss-proof version needs the receiver to report media bytes separately from total bytes;
    /// `VideoReceiveLog` is the place that would come from.
    MediaEfficiencyAtLeast(u8),
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
    pub received_bytes: u64,
    pub forwarded_media_bytes: u64,
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
    /// Publishers this viewer was *told* the SFU had stopped forwarding.
    ///
    /// A stream that stops is either paused or broken, and the media cannot tell you which. The
    /// difference decides whether a UI can show a placeholder or has to leave the tile blank.
    pub signalled_paused: BTreeSet<String>,
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
        Some(pct(self.received_bytes as f64, deliverable))
    }

    /// Congestion tail-drop as a percentage of packets offered to the bottleneck. Distinct from
    /// configured link loss: this is the controller overusing the link.
    pub fn congestion_loss_percent(&self) -> f64 {
        let offered = self.delivered_packets + self.congestion_drops;
        pct(self.congestion_drops as f64, offered as f64)
    }

    /// Media payload as a percentage of bytes received. Exceeds 100% under loss - see
    /// [`Property::MediaEfficiencyAtLeast`].
    #[allow(dead_code)] // Report accessor, kept alongside the rest of the surface.
    pub fn media_percent(&self) -> f64 {
        pct(
            self.forwarded_media_bytes as f64,
            self.received_bytes as f64,
        )
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
        undecodable_keyframes: video.missing_parameter_sets,
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
        received_bytes: handle
            .rx_bytes()
            .saturating_sub(handle.interval_rx_baseline),
        forwarded_media_bytes: handle.forwarded_media(),
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
        signalled_paused: handle.paused_publishers(),
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
    let received = handle
        .rx_bytes()
        .saturating_sub(handle.interval_rx_baseline);
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
    match capacity {
        // Utilisation needs a capacity that held for the window. On a schedule the instantaneous
        // value is the wrong denominator and produces nonsense - a ramp ending at 700 kbps whose
        // window averaged ~1.6 Mbps reported 137% - so report the rate and omit the ratio.
        Some(c) if !fixed => out.push_str(&format!(
            "capacity {c} bps (scheduled) | received {received} B"
        )),
        Some(c) => {
            let deliverable = c as f64 * window.as_secs_f64() / 8.0;
            out.push_str(&format!(
                "capacity {c} bps | utilisation {:.1}% ({received} B / ~{deliverable:.0} B)",
                pct(received as f64, deliverable)
            ));
        }
        None => out.push_str(&format!("capacity unshaped | received {received} B")),
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
        " | qoe {} fps={:.1} key={} undecodable={} torn={} ttff={:?} freeze={:?}/{:.0}% | queue standing {:?} max {:?} | congestion loss {:.2}% ({}/{offered}) | link loss {}          | media {:.1}%",
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
        pct(forwarded as f64, received as f64),
    ));
    out
}

/// Evaluate `property`, returning a human-readable reason on failure.
fn check_property(
    property: &Property,
    handle: &ParticipantHandle,
    ip: IpAddr,
    window: Duration,
    handles: &PlanHandles,
) -> Result<(), String> {
    let now = tokio::time::Instant::now();
    let stats = pulsebeam_runtime::net::shaper::stats(ip);

    let capacity = || -> Result<f64, String> {
        if !pulsebeam_runtime::net::shaper::capacity_is_fixed(ip) {
            return Err(
                "this property needs a fixed capacity, but the link is on a schedule".to_string(),
            );
        }
        pulsebeam_runtime::net::shaper::capacity_at(ip, now)
            .map(|c| c as f64)
            .ok_or_else(|| "this property needs a shaped link, but none is configured".to_string())
    };

    let series = || -> Result<Vec<(Duration, u64, u64)>, String> {
        let id = handle
            .shared
            .participant_id
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "participant never connected".to_string())?;
        let s = pulsebeam::sim_metrics::bwe_series(&id);
        if s.is_empty() {
            // Distinguishing this from "passed" matters: a subscriber that ran no allocation
            // passes satisfies every bound vacuously.
            return Err(format!(
                "no allocation passes recorded for {id}; the check would pass vacuously"
            ));
        }
        Ok(s)
    };

    match *property {
        Property::VideoDecodes => {
            let video = handle.video_stats_since_interval();
            let total = handle.video_rx().stats();
            if video.frames == 0 {
                return Err("no video frames were decoded during the run".to_string());
            }
            if total.keyframes == 0 {
                return Err("no video keyframe was decoded".to_string());
            }
            if total.missing_parameter_sets != 0 {
                return Err(format!(
                    "{} decoded keyframes were missing parameter sets",
                    total.missing_parameter_sets
                ));
            }
        }
        Property::UtilisationAtLeast(min) => {
            let capacity = capacity()?;
            let received = handle
                .rx_bytes()
                .saturating_sub(handle.interval_rx_baseline);
            let deliverable = capacity * window.as_secs_f64() / 8.0;
            let got = pct(received as f64, deliverable);
            if got + 0.5 < min as f64 {
                return Err(format!(
                    "used {got:.1}% of the link ({received} B of ~{deliverable:.0} B deliverable \
                     at {capacity:.0} bps over {window:?}); expected >= {min}%"
                ));
            }
        }
        Property::EstimateMeetsNeed { percent } => {
            let series = series()?;
            // Steady-state demand, not the transient early value: the allocator's `desired`
            // starts at a floor and rises as it learns the ladder, so the last sample is the
            // honest statement of what was wanted.
            let (_, last_bwe, last_desired) = *series.last().expect("non-empty");
            let capacity = pulsebeam_runtime::net::shaper::capacity_at(ip, now).unwrap_or(u64::MAX);
            let need = last_desired.min(capacity);
            if need == 0 {
                return Err("nothing was demanded, so the claim would be vacuous".to_string());
            }
            let got = pct(last_bwe as f64, need as f64);
            if got + 0.5 < percent as f64 {
                return Err(format!(
                    "estimate ended at {last_bwe} bps against a need of {need} bps \
                     (demand {last_desired}, capacity {capacity}) — {got:.1}%; expected \
                     >= {percent}%"
                ));
            }
        }
        Property::EstimateTracksCapacity { within_percent } => {
            let capacity = capacity()?;
            let series = series()?;
            let last = series.last().expect("non-empty").1 as f64;
            let off = (last - capacity).abs() / capacity * 100.0;
            if off > within_percent as f64 {
                return Err(format!(
                    "estimate ended at {last:.0} bps against {capacity:.0} bps of capacity \
                     ({off:.1}% off); expected within {within_percent}%"
                ));
            }
        }
        Property::EstimateStable { max_drop_percent } => {
            // A plan that delivered nothing satisfies "the estimate did not fall" for the worst
            // possible reason: a frozen estimate never moves. Caught in practice - a plan whose
            // publisher went dead reported a 0.0% drawdown over 600 samples having received 0
            // bytes, and passed.
            let received = handle
                .rx_bytes()
                .saturating_sub(handle.interval_rx_baseline);
            if received == 0 {
                return Err(
                    "nothing was received, so a stable estimate means nothing moved at all"
                        .to_string(),
                );
            }
            let series = series()?;
            let mut peak = 0.0f64;
            let mut worst = 0.0f64;
            let mut worst_at = Duration::ZERO;
            let mut worst_from = 0.0f64;
            let mut worst_to = 0.0f64;
            for (at, bps, _) in &series {
                let bps = *bps as f64;
                peak = peak.max(bps);
                if peak > 0.0 {
                    let drop = (peak - bps) / peak * 100.0;
                    if drop > worst {
                        worst = drop;
                        worst_at = *at;
                        worst_from = peak;
                        worst_to = bps;
                    }
                }
            }
            if worst > max_drop_percent as f64 {
                return Err(format!(
                    "estimate fell {worst:.1}% ({worst_from:.0} -> {worst_to:.0} bps) at \
                     {worst_at:?} into the window; expected no drop over {max_drop_percent}%"
                ));
            }
        }
        Property::EstimateRecovers { of_peak_percent } => {
            let received = handle
                .rx_bytes()
                .saturating_sub(handle.interval_rx_baseline);
            if received == 0 {
                return Err(
                    "nothing was received, so a recovered estimate means nothing moved at all"
                        .to_string(),
                );
            }
            let series = series()?;
            let last = series.last().expect("non-empty").1 as f64;
            let peak = series.iter().map(|(_, b, _)| *b).max().unwrap_or(0) as f64;
            if peak <= 0.0 {
                return Err("the estimate never rose above zero".to_string());
            }
            let got = last / peak * 100.0;
            if got + 0.5 < of_peak_percent as f64 {
                return Err(format!(
                    "estimate ended at {last:.0} bps against a peak of {peak:.0} ({got:.1}% of \
                     it); expected to recover to within {of_peak_percent}%"
                ));
            }
        }
        Property::EstimateConverges { percent, within } => {
            let capacity = capacity()?;
            let series = series()?;
            let target = capacity * percent as f64 / 100.0;
            match series.iter().find(|(_, bps, _)| *bps as f64 >= target) {
                Some((at, _, _)) if *at <= within => {}
                Some((at, _, _)) => {
                    return Err(format!(
                        "reached {percent}% of capacity ({target:.0} bps) only after {at:?}; \
                         expected within {within:?}"
                    ));
                }
                None => {
                    let best = series.iter().map(|(_, b, _)| *b).max().unwrap_or(0);
                    return Err(format!(
                        "never reached {percent}% of capacity ({target:.0} bps); best was \
                         {best} bps against {capacity:.0} bps of link"
                    ));
                }
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
        Property::PeakQueueingDelayBelow(max) => {
            if stats.max_backlog > max {
                return Err(format!(
                    "bottleneck queue reached {:?}; expected below {max:?} \
                     (standing at {:?})",
                    stats.max_backlog,
                    stats.mean_backlog()
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
        Property::QualityChangesPerMinuteBelow { origin, max } => {
            // Resolved here rather than read off the report: the report's map is filled by the
            // end-of-plan scoreboard, which has not run yet mid-plan.
            let Some(id) = handles
                .get(origin)
                .and_then(ParticipantHandle::participant_id)
            else {
                return Err(format!(
                    "{origin} has no runtime participant id yet; add a Step::Run before this step"
                ));
            };
            let changes = pulsebeam::sim_metrics::quality_changes(&id);
            let minutes = window.as_secs_f64() / 60.0;
            if minutes <= 0.0 {
                return Err("the window has no duration to rate-limit against".to_string());
            }
            let rate = changes as f64 / minutes;
            if rate > max as f64 {
                return Err(format!(
                    "{origin}'s layer changed {changes} times in {window:?} ({rate:.0}/min);                      expected at most {max}/min. Each change costs a keyframe and stutters the                      picture, so at this rate the stream never holds a layer long enough to                      decode and the viewer sees nothing at all"
                ));
            }
        }
        Property::QualityReversalsBelow { origin, max } => {
            let Some(id) = handles
                .get(origin)
                .and_then(ParticipantHandle::participant_id)
            else {
                return Err(format!(
                    "{origin} has no runtime participant id yet; add a Step::Run before this step"
                ));
            };
            let reversals = pulsebeam::sim_metrics::quality_reversals(&id);
            if reversals > max {
                let changes = pulsebeam::sim_metrics::quality_changes(&id);
                return Err(format!(
                    "{origin}'s layer reversed direction {reversals}x in {window:?} \
                     ({changes} changes total); expected at most {max}. Climbing through layers is \
                     fine, but reversing means the stream is oscillating rather than settling, and \
                     every switch costs a keyframe and stutters the picture"
                ));
            }
        }
        Property::NeverForwardedAbove {
            origin,
            max_quality,
        } => {
            let Some(id) = handles
                .get(origin)
                .and_then(ParticipantHandle::participant_id)
            else {
                return Err(format!(
                    "{origin} has no runtime participant id yet; add a Step::Run before this step"
                ));
            };
            let Some(peak) = pulsebeam::sim_metrics::max_forwarded_quality(&id) else {
                return Err(format!(
                    "no allocation pass recorded {origin}, so nothing was measured and this \
                     would pass vacuously"
                ));
            };
            if peak > max_quality {
                return Err(format!(
                    "{origin} was forwarded at layer {peak}, above the requested ceiling of \
                     {max_quality}. A viewer asking for a short tile is asking not to be sent a \
                     taller one; sending it spends the link on pixels that will never be shown"
                ));
            }
        }
        Property::MediaEfficiencyAtLeast(min) => {
            let received = handle
                .rx_bytes()
                .saturating_sub(handle.interval_rx_baseline);
            if received == 0 {
                return Err("nothing was received, so the ratio would be meaningless".to_string());
            }
            let forwarded = handle.forwarded_media();
            let got = pct(forwarded as f64, received as f64);
            if got < min as f64 {
                return Err(format!(
                    "only {got:.1}% of received bytes were media ({forwarded} media / \
                     {received} received); expected >= {min}%"
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
    num_shards: usize,
    link: LinkProfile,
    buggify_permille: u32,
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
            // One shard for now, which is wrong and is meant to change.
            //
            // A single-shard node never crosses a shard boundary, so every
            // route, destination runtime and remote plan on the forwarding path
            // goes unexercised - which is how cross-shard audio stayed broken
            // through 103 passing plans. Raising this to 16 is a one-line
            // change that currently fails two plans, both pre-existing and both
            // reproduced by the ignored tests in `cross_shard.rs`. It becomes
            // the default once the publication catalog lands, since that is
            // what removes the shard-local key confusion behind them.
            num_shards: 1,
            buggify_permille: 0,
            link: LinkProfile::default(),
        }
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

    /// Inject failures at declared points, `permille` parts per thousand.
    ///
    /// Off everywhere else, so the rest of the suite keeps asserting against a system where
    /// nothing unexpected fails. A plan that turns this on is asserting something different: that
    /// the recovery paths hold, not that the happy path is correct.
    pub fn with_buggify(mut self, permille: u32) -> Self {
        self.buggify_permille = permille;
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
        self
    }

    /// Spread the node across N worker shards.
    ///
    /// Independent of [`Self::with_tcp_only`]. Over UDP the shard a datagram
    /// reaches is chosen by the `SO_REUSEPORT` group, which hashes its 4-tuple
    /// exactly as the kernel does — so a plan that sets this is also exercising
    /// the demuxer's shard check and the arrival path a real deployment uses.
    pub fn with_shards(mut self, n: usize) -> Self {
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
        crate::sim_rand::set_thread_rng(self.rng_seed);
        fastrand::seed(self.rng_seed);
        // Loss, reordering and duplication come from the shaper's own stream, not
        // from turmoil's RNG, so it has to be seeded too or a sweep replays one
        // impairment pattern under every seed.
        pulsebeam_runtime::net::shaper::seed_impairments(self.rng_seed);
        pulsebeam_runtime::buggify::enable(self.buggify_permille, self.rng_seed);
        tracing::info!(
            seed = self.rng_seed,
            "simulation plan seed; replay with `make test-sim-seed SEED={}`",
            self.rng_seed
        );

        let sim_duration = plan
            .iter()
            .filter_map(|s| match s {
                Step::Run { duration, .. } => Some(*duration),
                _ => None,
            })
            .sum::<Duration>()
            + Duration::from_secs(60);

        let mut sim = turmoil::Builder::new()
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
        let server_ip = subnet_ip(subnet, 1);
        let coordinator_ip = subnet_ip(subnet, 254);
        let seed = self.rng_seed;
        let tcp_only = self.tcp_only;
        let num_shards = self.num_shards;

        sim.host(server_ip, move || async move {
            let rng = pulsebeam_runtime::rand::seeded_rng(seed);
            start_sfu_node_with(server_ip, rng, num_shards, tcp_only)
                .await
                .map_err(Into::into)
        });

        let mut handles = PlanHandles::new();
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
                let ip = subnet_ip(subnet, ip_counter);
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

                let shared = Arc::new(ParticipantShared::new());
                let (cmd_tx, cmd_rx) = mpsc::channel::<ParticipantCmd>(16);

                handles.insert(
                    participant.name,
                    ParticipantHandle {
                        shared: shared.clone(),
                        cmd_tx,
                        interval_tx_baseline: 0,
                        interval_rx_baseline: 0,
                        subscribed_at: None,
                        interval_video_baseline: VideoReceiveStats::default(),
                        publishes_video: matches!(participant.role, Role::Publisher)
                            || participant.subscribes && !participant.rids.is_empty(),
                        publishes_audio: participant.audio_level_dbov.is_some(),
                        present: !participant.starts_disconnected,
                        departed_cleanly: true,
                        departures: Vec::new(),
                    },
                );

                let config = participant.clone();
                let room_name = room.name;

                sim.client(ip, async move {
                    run_participant(ip, server_ip, config, room_name, shared, cmd_rx, tcp_only)
                        .await
                        .map_err(Into::into)
                });
            }
        }

        let reports: Arc<Mutex<HashMap<&'static str, LinkReport>>> = Default::default();
        let reports_inner = reports.clone();
        sim.client(coordinator_ip, async move {
            let mut handles = handles;
            execute_plan(plan, &mut handles, &name_to_ip, &reports_inner)
                .await
                .map_err(Into::into)
        });

        let wall_budget = sim_duration * 3 + Duration::from_secs(120);
        run_sim_or_timeout(&mut sim, wall_budget).expect("simulation failed");

        reports.lock().expect("reports poisoned").clone()
    }
}
