# `pulsebeam-rtc` design

Status: normative implementation contract for the first production release.

This document contains the complete non-congestion-control design and public Rust
contract. `README.md` owns the high-level boundary and invariants.
`congestion-control.md` owns private congestion-control, allocation, pacing, probing,
and RTP/SCTP coordination details.

The words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are normative. Public Rust
names and representations are part of the initial contract; changing them requires
revising this document before implementation.

## Design decisions considered

This section records the consequential alternatives considered before implementation.
It is intentionally concise: implementations MUST follow the chosen contract rather
than reopen rejected alternatives for convenience.

| Topic | Decision | Alternatives considered | Why this direction |
| --- | --- | --- | --- |
| Public boundary | One `Connection` facade with `accept`, `receive`, `command`, `poll`, `stats` | Expose protocol/controller subsystems or many specialized methods | Keeps the crate deep and stable; low-level state remains replaceable and testable internally. |
| I/O ownership | Sans-I/O; caller owns sockets and scheduling | Let the crate own sockets/tasks | Required for shard ownership, deterministic simulation, and avoiding hidden runtime policy. |
| Send accounting | `Output::Transmit` is the irreversible sent commit | Departure receipts; async send acknowledgments; rollback | Smaller API and deterministic controller state. Caller-side send failure conservatively appears as loss. |
| TCP fallback | Same `Transmit` abstraction as UDP | Separate public TCP congestion/departure mode | ICE-TCP is fallback; exposing kernel/TCP queue semantics would complicate the core boundary without improving the primary UDP path. |
| Clock ownership | Runtime supplies paired `TimePoint { monotonic, global }` | Hidden clock reads; connection-owned NTP/PTP discipline; mutable clock-anchor API | Keeps clock discipline outside RTC, deterministic tests possible, and time domains explicit. |
| Global media time | `GlobalMediaTime` is cluster-global, serializable microseconds | Endpoint Absolute Capture Time; endpoint NTP epoch; cross-machine `Instant` | Endpoint clocks are not trusted and `Instant` is local-only. Server NTP/PTP provides one comparable cluster domain. |
| RTP/RTCP mapping | Edge ingress anchors RTP to server global time; SR relationships refine/synchronize streams | Trust SR absolute NTP epoch; wait for SR before media | Preserves startup and A/V relationships without trusting broken endpoint wall clocks. |
| Transit time | Preserve `global_media_at` exactly across shards/nodes | Rebase/reconstruct at every hop | Makes media age comparable cluster-wide and prevents relay-induced clock drift. |
| Source switching | Stable outbound `SenderId`; map `GlobalMediaTime` into one outbound RTP clock | Copy source RTP timestamps; create a new sender per source | Allows packet-boundary source/layer switches without RTP/RTCP/RTX/CC reset. |
| Frame semantics | SFU supplies `FrameMetadata`; RTC acts on it | Codec-parse inside RTC; expose only raw packets | SFU already knows source/layer semantics and opaque/SFrame payload may be unparseable; RTC still owns transport decisions. |
| Payload parsing | Core forwarding remains payload-opaque | Require codec payload parsing for all transport behavior | Preserves SFrame/opaque forwarding and keeps codec knowledge out of the connection core. |
| Packet sharing | Local fanout may shallow-clone; explicit transit deep-copies | Share refcounted packet payload across shard/node ownership | Avoids cross-core packet-runtime refcount sharing while retaining cheap same-owner fanout. |
| Session lifecycle | One immutable offer/answer session; changed session creates a new `Connection` | Trickle ICE, ICE restart, media renegotiation inside this boundary | Keeps lifecycle bounded and reconstruction declarative; these features can be added only with an explicit contract revision. |
| Feedback requirement | Outbound RTP requires TWCC or RFC 8888 | REMB; fixed-rate fallback; video-only requirement | One aggregate RTP controller needs packet-level feedback even for audio-only operation. |
| DataChannels | SCTP owns reliability/congestion; connection coordinates its bounded service with RTP | Feed SCTP bytes into SCReAM/TWCC; one pseudo-cwnd for all traffic | RTP feedback cannot acknowledge SCTP. Keeping controllers distinct avoids false accounting while still preventing starvation. |
| Error handling | Malformed peer traffic is generally dropped/counted; semantic caller errors are returned | Bubble low-level parser/protocol errors publicly | Keeps hostile input from expanding API/error state and prevents implementation types from leaking. |
| Resource policy | Explicit public limits plus fixed private hard bounds | Unbounded maps/queues; best-effort cleanup after overflow | Correctness and predictable per-connection work outrank preserving every packet/state under overload. |
| Browser acceptance | Chrome + Firefox live acceptance; stored SDP only parser evidence | Claim compatibility from standards/parser fixtures alone | Runtime behavior such as TWCC, RTX, probing, and DataChannels requires live evidence. |

## Minimal public surface

The normative surface is one stateful facade, typed input commands, typed
outputs, immutable media values, stable IDs, and coherent snapshots.
Convenience wrappers MAY be added only when they are exact forwards to this
surface and introduce no second semantics.

```rust
pub struct Connection { /* private */ }

pub struct AcceptedConnection {
    pub connection: Connection,
    pub answer: SdpAnswer,
    pub session: SessionInfo,
}

impl Connection {
    pub fn accept(
        config: ConnectionConfig,
        offer: SdpOffer,
        at: TimePoint,
        entropy: ConnectionEntropy,
    ) -> Result<AcceptedConnection, AcceptError>;

    pub fn receive(
        &mut self,
        at: TimePoint,
        input: NetworkInput,
    ) -> Result<(), ReceiveError>;

    pub fn command(
        &mut self,
        at: TimePoint,
        command: Command,
    ) -> Result<(), CommandError>;

    pub fn poll(&mut self, at: TimePoint) -> Output;

    pub fn stats(&self) -> StatsSnapshot;
}
```

All state changes occur through `receive`, `command`, or `poll`. The caller drains
`poll` until it returns `Idle` before sleeping or processing another connection.
`stats` performs no protocol work and reads one coherent snapshot.

### Time types

```rust
#[repr(transparent)]
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct GlobalMediaTime(u64);

#[derive(Clone, Copy)]
pub struct TimePoint {
    pub monotonic: std::time::Instant,
    pub global: GlobalMediaTime,
}

pub struct ConnectionEntropy([u8; 32]);
```

`ConnectionEntropy` is consumed by `Connection::accept`, expanded by a private
cryptographic generator, and zeroized after the per-connection DTLS identity,
ICE tie-breakers, RTP bases, and other required randomness are derived. The
caller MUST fill it from a cryptographically secure source and MUST NOT reuse it.
The crate exposes no RNG trait or provider-specific cryptographic type.

`GlobalMediaTime` is PulseBeam's cluster-global media-time domain:

- the integer is unsigned microseconds in the cluster's NTP-compatible global
  timescale;
- every node in one PulseBeam cluster MUST use the same epoch and clock
  discipline;
- its canonical wire encoding is exactly eight unsigned big-endian bytes;
- it is directly comparable across media kinds, participants, connections,
  shards, and nodes in the same cluster;
- it is not interchangeable with `Instant`, `SystemTime`, RTP timestamps, or an
  application timestamp;
- the runtime MUST derive it from a cluster clock synchronized by NTP/PTP or an
  equivalent mechanism and MUST present it monotonically to a live connection;
- a PTP/TAI source MUST be converted to the cluster's NTP-compatible timescale
  before constructing a `TimePoint`;
- crossing between clusters with different clock domains is unsupported unless
  an outer relay explicitly translates the domain.

The type provides only explicit arithmetic and wire conversion:

```rust
impl GlobalMediaTime {
    pub const fn from_micros(value: u64) -> Self;
    pub const fn as_micros(self) -> u64;
    pub const fn to_be_bytes(self) -> [u8; 8];
    pub const fn from_be_bytes(bytes: [u8; 8]) -> Self;

    pub fn checked_add(self, duration: Duration) -> Option<Self>;
    pub fn checked_sub(self, duration: Duration) -> Option<Self>;
    pub fn checked_duration_since(self, earlier: Self) -> Option<Duration>;
}
```

`TimePoint` is local and is never serialized. `monotonic` drives SCReAM, pacing,
RTT, retransmission, protocol timeouts, and `next_wakeup`. `global` anchors and
compares media timelines. A `Connection` never reads either clock itself.

For successive calls to one connection, `TimePoint.monotonic` and
`TimePoint.global` MUST NOT regress. In release builds either regressing field is
clamped to the last accepted value, counted, and surfaced once as an actionable
warning; the call otherwise proceeds from the clamped pair. Debug builds also
assert the caller contract. Large forward global corrections are accepted and may
conservatively expire old media. Clock slewing and step policy remain runtime
concerns.

### Stable IDs

```rust
#[repr(transparent)] pub struct SenderId(u16);
#[repr(transparent)] pub struct EncodingId(u32);
#[repr(transparent)] pub struct DataChannelId(u16);
#[repr(transparent)] pub struct IceTcpFlowId(u64);
#[repr(transparent)] pub struct FrameId(u64);
```

IDs are opaque values, not SSRCs, MIDs, RIDs, SCTP stream IDs, or array indexes in
caller code.

- `SenderId` identifies one negotiated outbound RTP sender. Its policy and wire
  identity survive source, encoding, primary SSRC, RTX SSRC, and layer changes.
- `EncodingId` identifies one inbound encoding. Negotiated encodings are stable;
  unsignaled encodings remain stable until RTCP BYE or explicit retirement.
- `FrameId` is supplied by the SFU with `FrameMetadata`. It MUST be unique among
  frames simultaneously queued or retained for retransmission on a given
  `SenderId`; it need not be globally unique or monotonic.

### Session support types

SDP text and negotiated facts are wrapped so raw parser/implementation types do
not escape:

```rust
pub struct SdpOffer(Arc<str>);
pub struct SdpAnswer(Arc<str>);

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum MediaKind {
    Audio,
    Video,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum PacketFeedbackKind {
    TransportWide,
    Rfc8888,
}

pub struct SessionInfo {
    pub feedback: Option<PacketFeedbackKind>,
    pub senders: Arc<[SenderInfo]>,
}

pub struct SenderInfo {
    pub id: SenderId,
    pub kind: MediaKind,
    pub mid: Arc<str>,
    pub rtp_clock_rate: u32,
    pub supports_rtx: bool,
    pub signals_playout_delay: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum EcnCodepoint {
    NotEct,
    Ect0,
    Ect1,
    Ce,
}
```

`SessionInfo` is immutable. It reports only stable negotiated facts needed by the
SFU; payload types, SSRCs, ICE credentials, fingerprints, RTCP parser values, and
extension wire IDs remain connection internals.

### Network input and output

```rust
pub enum NetworkInput {
    Udp {
        local: SocketAddr,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
        payload: Bytes,
    },
    IceTcp {
        flow: IceTcpFlowId,
        local: SocketAddr,
        remote: SocketAddr,
        frame: Bytes,
    },
}

pub enum TransmitTarget {
    Udp {
        local: SocketAddr,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
    },
    IceTcp {
        flow: IceTcpFlowId,
    },
}

pub struct Transmit {
    pub target: TransmitTarget,
    pub payload: Bytes,
}
```

Each UDP value contains exactly one datagram. Each ICE-TCP value contains exactly
one complete RFC 4571 frame, including its two-byte frame length. The first frame
for an `IceTcpFlowId` binds that caller-owned accepted stream to its local/remote
candidate tuple; later frames MUST repeat the same tuple. The caller owns stream
reassembly before `receive` and stream writes after `poll`.

Emitting `Output::Transmit` is the connection's irrevocable transmission commit:

1. the packet's transport sequence number and RTP/RTCP continuity are committed;
2. the packet enters sent history and bytes-in-flight at `TimePoint.monotonic`;
3. pacing tokens and sender service are charged;
4. forwarding latency is measured through this commit point;
5. the caller MUST attempt socket submission before calling the connection again.

There is no transmit receipt and no `report_departure` API. The connection assumes
every emitted `Transmit` was submitted. A discarded value, failed syscall, or
kernel drop therefore appears to feedback as loss or missing feedback. The caller
MUST NOT place `Transmit` in another application queue or retry it through the
connection. Runtime poll-to-syscall latency is measured and governed outside this
crate.

UDP and fallback ICE-TCP use the same commit semantics. A TCP write can remain in
the kernel's TCP queue; `pulsebeam-rtc` does not claim to observe physical NIC or
wire departure and does not expose a second TCP congestion mode.

### Commands

```rust
#[non_exhaustive]
pub enum Command {
    SetSenderPolicy {
        sender: SenderId,
        policy: SenderPolicy,
    },
    SendMedia {
        sender: SenderId,
        media: ForwardedMedia,
    },
    RequestKeyframe {
        encoding: EncodingId,
    },
    RetireEncoding {
        encoding: EncodingId,
    },
    OpenDataChannel(DataChannelConfig),
    SendData {
        channel: DataChannelId,
        message: DataMessage,
    },
    CloseDataChannel {
        channel: DataChannelId,
    },
    CloseGracefully {
        deadline: Instant,
    },
    Abort,
}
```

`SendMedia` transfers one immutable packet reference into egress admission. A
successful command means the packet was accepted for immediate transmission or
bounded queueing; it does not mean the packet will necessarily survive later
congestion or deadline shedding. A typed `WouldBlock` is returned before taking
ownership when a configured bound would be exceeded.

`CloseGracefully` rejects new application work, continues required protocol
shutdown traffic, and becomes closed no later than its deadline. `Abort` drops all
state immediately and emits no further transmit.

### Poll output

```rust
#[non_exhaustive]
pub enum Output {
    Transmit(Transmit),
    Event(Event),
    Idle {
        next_wakeup: Option<Instant>,
    },
    Closed(CloseReason),
}
```

One call returns at most one output. `Closed(reason)` is emitted exactly once and
is terminal. `Idle.next_wakeup` is the only externally scheduled connection timer
and is the earliest local monotonic instant at which calling `poll` can make
progress without new input. `None` means only external input can make progress. A
connection does work proportional to new input, expired bounded state, or one
emitted output; it never scans unrelated connections.

## Connection configuration

The public configuration contains capabilities, semantic defaults, and resource
budgets only. It does not expose SCReAM gains, queue targets, probe clusters,
pacer factors, feedback filters, or controller selection.

```rust
pub struct ConnectionConfig {
    pub capabilities: SessionCapabilities,
    pub limits: ConnectionLimits,
    pub default_audio_policy: SenderPolicy,
    pub default_video_policy: SenderPolicy,
    pub unknown_extension_policy: UnknownExtensionPolicy,
}

pub struct ConnectionLimits {
    pub max_unsignaled_encodings: u16,
    pub max_data_channels: u16,
    pub max_inbound_data_message_bytes: usize,
    pub max_buffered_data_bytes: usize,
    pub max_queued_media_bytes: usize,
    pub max_retransmission_bytes: usize,
}
```

Version-one defaults and hard maxima are:

| Limit | Default | Hard maximum |
| --- | ---: | ---: |
| Unsignaled encodings | 32 | 1,024 |
| DataChannels | 256 | 4,096 |
| One inbound DataChannel message | 1 MiB | 16 MiB |
| Total buffered DataChannel payload | 8 MiB | 64 MiB |
| Queued outbound media payload | 8 MiB | 64 MiB |
| Retained retransmission payload | 16 MiB | 128 MiB |

A configured value above a hard maximum is rejected at construction. Internal
packet counts, feedback histories, timers, and queue horizons have additional
fixed bounds in the congestion-control contract and are not public knobs.

## Events

```rust
#[non_exhaustive]
pub enum Event {
    Connected,
    Media {
        encoding: EncodingId,
        packet: MediaPacket,
    },
    EncodingDiscovered(EncodingInfo),
    EncodingRetired {
        encoding: EncodingId,
        reason: EncodingRetireReason,
    },
    KeyframeRequested {
        sender: SenderId,
    },
    AllocationChanged(AllocationSnapshot),
    DataChannel(DataChannelEvent),
    Warning(ConnectionWarning),
}
```

Events are semantic and bounded. There are no per-packet congestion events.
Repeated malformed input, unknown feedback, drops, and controller diagnostics are
counters in `StatsSnapshot`; `Warning` is reserved for actionable state changes
that a caller may need to surface.

`AllocationSnapshot` contains the governed aggregate RTP media-payload capacity
and one allocation for each active sender. Downward safety changes are emitted
immediately. Upward/noise changes use the fixed hysteresis in the detailed
contract.

## Public error types

Peer-controlled malformed packets are normally dropped and counted rather than
returned as API errors. Public errors describe caller input, negotiation, or a
terminal connection condition.

```rust
#[non_exhaustive]
pub enum AcceptError {
    InvalidOffer,
    UnsupportedSessionProfile,
    MissingPacketFeedback,
    CapabilityConflict,
    SessionLimitExceeded,
    InvalidConfiguration,
    CryptographicFailure,
}

#[non_exhaustive]
pub enum ReceiveError {
    Closed,
    UnknownIceTcpFlow,
    InvalidNetworkEnvelope,
    InputLimitExceeded,
}

#[non_exhaustive]
pub enum CommandError {
    Closed,
    UnknownSender(SenderId),
    UnknownEncoding(EncodingId),
    UnknownDataChannel(DataChannelId),
    InvalidPolicy(PolicyError),
    InvalidFrameMetadata,
    InvalidState,
    MessageTooLarge,
    WouldBlock,
}

#[non_exhaustive]
pub enum PolicyError {
    PlayoutRange,
    PlayoutNotExactlyRepresentable,
    PriorityOutOfRange,
    BitrateOutOfRange,
}
```

Low-level parser, SRTP, SCTP, SCReAM, TWCC, and RTCP errors are translated into
these semantic categories or into bounded statistics. They are never wrapped as
public implementation errors.

## Statistics

`stats()` returns an owned coherent snapshot and never registers a metrics
backend.

```rust
pub struct StatsSnapshot {
    pub connection: ConnectionStats,
    pub senders: Vec<SenderStats>,
    pub encodings: Vec<EncodingStats>,
    pub data_channels: Vec<DataChannelStats>,
}
```

Required connection observations include:

- connection state, selected path kind, negotiated packet-feedback kind, and
  close reason;
- RTP media-payload desired, allocated, admitted, and transmitted rates;
- emitted transport-byte rate split into RTP/RTCP, SCTP, DTLS/ICE, and padding;
- controller target media-payload rate, governed media capacity, allowed and
  actual RTP bytes-in-flight, and pacing rate;
- paced queue bytes/packets, predicted pacer delay, and oldest queued media age;
- baseline delay, queue delay, smoothed RTT, feedback hold, delivery rate, loss,
  and estimator confidence;
- application-limited, feedback-stale, probing, and congestion classification;
- probe attempts, useful observations, bytes, aborts, and reasons;
- pre-admission frame drops, post-admission packet drops, deadline misses, RTX
  sent/skipped/recovered, and dependency-caused shedding;
- clock regressions, RTP-clock discontinuities, synchronization state counts, and
  global-media-time uncertainty;
- malformed/authentication/replay/unknown feedback counters;
- current and peak bounded-resource use.

Required per-sender observations include:

- playout range, whether the current signaled value is acknowledged, priority,
  desired media-payload rate, governed demand, allocation, and actual rate;
- active/application-limited classification;
- queue bytes/packets, oldest media age, private effective admission and RTX
  horizons, service balance, and allocation-change reason;
- source switches, timestamp-clamp drops, frame/dependency drops, deadline misses,
  and RTX outcomes.

Required per-encoding observations include RTP/RTCP packet/rate/loss data,
`GlobalMediaTime` mapping state, clock-segment count, synchronization group/status,
and discovery/retirement reason. Required per-channel observations include state,
priority, reliability, buffered amount, message counts/sizes, SCTP service delay,
and head-of-line blocking.

Private controller names and mutable internal objects are not returned.


## Session and interoperability contract

- The session is one immutable ICE-lite remote-offer/local-answer exchange.
- BUNDLE, RTCP mux, inline candidates, all SDP media directions, UDP, and passive
  ICE-TCP over IPv4 and IPv6 are supported.
- Trickle ICE, ICE restart, media renegotiation, and a TURN client are outside this
  boundary. A changed session creates a new connection. Client relay candidates
  remain usable.
- `Connection::accept` creates a fresh ephemeral DTLS identity from the supplied
  cryptographic entropy. Its private key is never exported or reused.
- Codecs and RTP header extensions are negotiated from immutable
  `SessionCapabilities`.
- Core forwarding does not require codec-payload parsing and MUST carry opaque or
  SFrame-protected media.
- Every accepted outbound RTP session, including audio-only RTP, MUST negotiate
  one supported packet-feedback mode: transport-wide congestion-control feedback
  or RFC 8888 congestion-control feedback. Absence is a specific
  `AcceptError::MissingPacketFeedback`; there is no REMB or fixed-rate fallback.
- The selected packet-feedback mode is immutable and applies to every bundled
  outbound RTP sender. TWCC and RFC 8888 are not run concurrently for the same
  RTP path.
- Compatibility means compatibility with the negotiated PulseBeam WebRTC
  profile, not every theoretically standards-compliant SDP. Live acceptance
  evidence covers pinned current Chrome and Firefox versions. Stored SDP is parser
  regression evidence only. Safari and `webrtcbin` are not initial acceptance
  targets.

The maximum accepted session is 128 negotiated media sections. The implementation
allocates dense storage from the accepted answer; it does not reserve runtime
encoding, RTX, probe, or DataChannel entities as media slots.


## Media packet contract

After ingress authentication and decryption, one immutable `Bytes` allocation is
the canonical plaintext RTP packet. Parsing stores compact byte ranges; semantic
accessors decode metadata lazily. Local fanout uses shallow clones. `to_transit`
performs an explicit deep copy before a packet crosses a shard or node boundary,
so packet reference counts never become shared runtime state across cores.

```rust
#[derive(Clone)]
pub struct MediaPacket { /* private */ }

impl MediaPacket {
    pub fn bytes(&self) -> &Bytes;
    pub fn global_media_at(&self) -> GlobalMediaTime;
    pub fn extension(&self, uri: &str) -> Option<&[u8]>;
    pub fn to_transit(&self) -> MediaPacket;
}

pub struct ForwardedMedia {
    pub packet: MediaPacket,
    pub frame: FrameMetadata,
}
```

`MediaPacket.global_media_at` is immutable. A relay MUST serialize and preserve it
exactly; an egress connection never reconstructs or rebases a transit packet's
value. The canonical relay representation is the eight-byte
`GlobalMediaTime::to_be_bytes()` followed by relay-owned framing for the packet
and frame metadata.

### Frame metadata supplied by the SFU

The SFU owns semantic knowledge that cannot reliably be recovered from opaque
payload. `pulsebeam-rtc` owns the transport action made possible by that
knowledge.

```rust
#[derive(Clone)]
pub struct FrameMetadata {
    pub id: FrameId,
    pub boundary: FrameBoundary,
    pub random_access: bool,
    pub discardable: bool,
    pub dependencies: FrameDependencies,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum FrameBoundary {
    Complete,
    Start,
    Middle,
    End,
}

#[derive(Clone)]
pub enum FrameDependencies {
    Unknown,
    Known(Arc<[FrameId]>),
}
```

`Known` contains at most eight direct dependencies. An empty list means the frame
is independently decodable in the caller's model. `Unknown` means the connection
may still admit or discard the complete current frame but MUST NOT infer which
later frames become undecodable.

For every packet in one frame:

- `id`, `random_access`, `discardable`, and `dependencies` MUST be identical;
- `global_media_at` MUST be identical;
- a one-packet frame uses `Complete`;
- a multi-packet frame uses one `Start`, zero or more `Middle`, and one `End`;
- dependencies MUST NOT contain the current frame ID;
- metadata inconsistency is `CommandError::InvalidFrameMetadata` and rejects the
  packet without mutating RTP continuity.

The caller SHOULD pass dependency information from a negotiated dependency
descriptor or codec-aware source selector. It MUST NOT make `pulsebeam-rtc` parse
protected codec payload merely to recover facts already known by the SFU.

`pulsebeam-rtc` uses this metadata for whole-frame admission, stale-frame
shedding, dependency-aware discard where known, keyframe protection, RTX
usefulness, and outbound dependency-descriptor continuity. It never chooses the
source or layer.

## Cluster-global media timeline

Every first-ingress RTP stream is normalized into the same `GlobalMediaTime`
domain. This is server-anchored media time, not trusted endpoint wall time and not
a claim of exact physical camera capture time.

### Edge ingress

For a packet arriving directly from an endpoint:

1. the first accepted RTP timestamp is provisionally anchored to
   `TimePoint.global` at server arrival;
2. the RTP clock rate maps later unwrapped RTP timestamps into microseconds, so
   packet-arrival jitter does not become media-timeline jitter;
3. RTCP Sender Reports refine the RTP-to-media mapping and relate audio/video RTP
   clocks in one RTCP synchronization group;
4. the endpoint's absolute NTP epoch is not trusted. Only differences and the
   relationship between RTP clocks are used;
5. previously emitted `global_media_at` values are immutable;
6. corrections affect future mapping through bounded slew and never make the
   mapping for increasing media time run backward.

For one continuous RTP-clock segment, the mapping from unwrapped RTP timestamp to
`GlobalMediaTime` is monotonic. Packets can arrive reordered and therefore an
older reordered packet may legitimately carry an earlier `global_media_at` than
a packet emitted before it.

If an encoder restarts or the RTP timestamp jumps beyond the accepted
reordering/drift model, the connection starts a new internal clock segment. The
new segment is anchored to the earliest reasonable global time not earlier than
the preceding segment's next expected media time. `EncodingId` need not change.

Before a usable Sender Report exists, media continues with a provisional mapping.
Cross-stream synchronization is then marked `Provisional`. Valid SR relationships
move it to `Synchronized`. Missing, contradictory, or discontinuous reports move
it to `Unverified` or `Discontinuous` without stopping media.

RTCP SDES CNAME and valid SR relationships determine synchronization groups.
Audio and video in one group are normalized by `pulsebeam-rtc`; the SFU receives
packets already expressed in one cluster-global domain.

For a usable SR pair `(sender_ntp, rtp_timestamp)`, the connection first evaluates
the provisional RTP mapping at `rtp_timestamp` to obtain `provisional_global`.
The group offset is the filtered difference:

```text
group_offset = provisional_global - sender_ntp
mapped_global = sender_ntp_for_packet + group_offset
```

Only sender-NTP differences are used. The absolute sender epoch cancels through
the offset. Offset and residual arithmetic uses signed 128-bit fixed-point
microseconds; an endpoint NTP value is never cast directly to
`GlobalMediaTime`. The first accepted stream establishes the synchronization-group
offset; later streams join it. Residuals are filtered over at least three SRs and
one RTT before a stream becomes `Synchronized`. A residual above `100 ms` or a
sender-NTP regression starts bounded revalidation instead of stepping emitted
media time.

### `MediaClockProfileV1`

The ingress mapper has one private fixed profile:

| Field | Version-one value |
| --- | ---: |
| RTP sequence reordering window used by the clock mapper | `2,048` packets |
| SRs required before `Synchronized` | `3` valid reports spanning at least `1` smoothed RTT |
| Maximum SR residual accepted for normal slew | `100 ms` |
| Maximum mapping correction slew | `1,000 ppm` relative to the negotiated RTP clock rate |
| Sender-NTP regression tolerance | `1 ms`; larger regression starts revalidation |
| SR stale interval | `10 s` without a valid report |
| RTP/global discontinuity threshold | `500 ms` from the current predicted mapping after reordering is excluded |
| Maximum clock segments retained per encoding | `4`; older completed segments collapse into statistics |

A correction within `100 ms` changes only the rate/offset used for future RTP
positions and is absorbed at no more than `1,000 ppm`. A larger residual, an RTP
timestamp jump beyond the discontinuity threshold, or contradictory sender-NTP
progression starts a new segment. Existing packets are never rewritten. For a
new stream joining a synchronization group before its first emitted media, the
validated group offset can be applied immediately because no public timestamp is
yet immutable.

### PulseBeam transit

For a packet received from another PulseBeam shard or node, the incoming
`global_media_at` is authoritative and MUST be preserved byte-for-byte. The local
connection does not inspect the original endpoint clock or recreate its RTCP
mapping.

### Cross-participant meaning

All packets are comparable in the same server clock domain. Equal values mean
equal positions in PulseBeam's normalized cluster timeline. Because first-edge
anchoring includes unknown endpoint-to-edge delay, equal values do not prove that
two cameras exposed a frame at exactly the same physical instant. This uncertainty
is observable in statistics and never converted into false capture-time claims.

## Inbound encodings

- Negotiated inbound encodings are admitted and receive stable `EncodingId`
  values.
- An authenticated unsignaled SSRC or RID emits `EncodingDiscovered` before its
  first `Media` event.
- Unsignaled encodings consume the configured connection-wide budget. Overflow
  is dropped and counted without closing the connection.
- Inactivity never retires a paused encoding. RTCP BYE or
  `Command::RetireEncoding` does.
- Malformed or unauthenticated packets never create an encoding.
- RTP clock state, RTCP synchronization state, retransmission input state, and
  lazy extension metadata belong to the encoding, not the SFU router.

## Outbound senders and source switching

`Command::SendMedia` can select a different source packet at any packet boundary.
The connection preserves the negotiated outbound sender's SSRC, payload type,
sequence-number space, RTP timestamp mapping, header-extension state, RTX state,
RTCP reports, and congestion policy.

An outbound sender creates one mapping from cluster-global media time to its RTP
clock:

```text
outbound_rtp_timestamp = base_rtp_timestamp
                       + round((global_media_at - base_global_media_at)
                               * rtp_clock_rate / 1_000_000)
```

All packets of one frame use the same mapped RTP timestamp. Source RTP timestamps
are never copied as outbound continuity. Switching from camera A to camera B is
therefore safe even when their original RTP timestamps and SSRCs are unrelated.

A packet whose global media position would move an already committed outbound
frame backward is stale and is rejected or shed; the connection never rewinds the
outbound RTP clock. A long pause advances RTP time according to elapsed
`GlobalMediaTime` rather than compressing the pause.

Outgoing RTCP Sender Reports use the same global clock mapping for all senders so
receiver-side A/V synchronization remains consistent. The runtime's NTP-compatible
`GlobalMediaTime` is converted to the RTCP 64-bit NTP representation internally;
no NTP type is exposed.

## Sender policy

Every negotiated outbound sender has one mutable semantic policy. Configuration
provides the initial policy, so no sender starts undefined.

```rust
pub struct SenderPolicy {
    pub playout_delay: PlayoutDelay,
    pub priority: MediaPriority,
    pub desired_bitrate: MediaPayloadBitrate,
}

#[repr(transparent)]
pub struct MediaPayloadBitrate(u64); // bits per second of RTP media payload

#[repr(transparent)]
pub struct MediaPriority(NonZeroU16);

pub struct PlayoutDelay {
    min_ticks: u16,
    max_ticks: u16,
}

impl MediaPayloadBitrate {
    pub const fn from_bps(value: u64) -> Self;
    pub const fn as_bps(self) -> u64;
}

impl MediaPriority {
    pub fn new(weight: u16) -> Result<Self, PolicyError>;
    pub const fn weight(self) -> u16;
}
```

### Playout delay

One tick is exactly 10 ms. Both values are in `0..=4095`, corresponding to
`0..=40_950 ms`, and `min_ticks <= max_ticks`. Construction rejects values that
cannot be represented exactly; it never silently rounds or clamps.

```rust
impl PlayoutDelay {
    pub fn from_ticks(min: u16, max: u16) -> Result<Self, PolicyError>;
    pub fn from_millis_exact(min: u64, max: u64) -> Result<Self, PolicyError>;
    pub fn min(self) -> Duration;
    pub fn max(self) -> Duration;
}
```

`0/0` means render as soon as practical. It is not a literal zero network or
render deadline.

When the playout-delay RTP extension is negotiated, the connection writes the
current value for that sender. After a change it repeats the value until an RTCP
receiver report proves, using wrap-aware outbound RTP sequence progression, that
a packet carrying the new value was received. TWCC acknowledgment alone is not
used as proof of extension delivery. When the extension is absent, the local
policy still governs admission and statistics mark it `NotSignaled`.

The immutable packet timestamp and mutable receiver policy have separate roles:

```text
MediaPacket.global_media_at       cluster-global media position
SenderPolicy.playout_delay        receiver-specific render intent
private receiver deadline         derived by the egress connection
```

The connection derives private receiver-specific handoff, recovery, and stale
media horizons from `global_media_at`, the playout range, receiver/network
estimates, and fixed uncertainty reserves. No receiver deadline is serialized
back into `MediaPacket`.

Tightening a policy immediately recomputes queued not-yet-started frames from
their immutable `global_media_at`, removes newly obsolete RTX, and tightens shared
path latency constraints. Relaxing is damped and never resurrects dropped media.

### Desired bitrate

`desired_bitrate` is the RTP media-payload rate the SFU could use if given the
capacity, after choosing a source or layer. Zero means intentionally inactive.
It is not a network estimate and does not include RTP, SRTP, UDP/TCP, RTCP, or
DataChannel overhead.

The connection measures admitted and transmitted rates itself. Allocation events
are also expressed in RTP media-payload bits per second, so the SFU can compare
them directly to layer payload rates.

### Media priority

`MediaPriority` is a bounded positive relative weight in `1..=256`. Zero is
invalid. The standard presets are:

```rust
impl MediaPriority {
    pub const VERY_LOW: Self = Self::new_const(1);
    pub const LOW: Self = Self::new_const(2);
    pub const MEDIUM: Self = Self::new_const(4);
    pub const HIGH: Self = Self::new_const(8);
}
```

The allocator interprets weights over RTP payload bytes. They are shares under
constraint, not reservations or guaranteed minimums. Media kind is not an
immutable priority rule; configuration may default audio to `HIGH` and video to
`MEDIUM`.

## RTP extension policy

Extension policy is immutable per negotiated sender and keyed by semantic URI,
never a source wire ID.

- MID, RID, repaired RID, transport-wide sequence number, absolute send time,
  playout delay, and dependency descriptors requiring outbound continuity are
  generated or rewritten by the connection.
- Known endpoint-independent values such as audio level and video orientation can
  be forwarded after URI-based remapping.
- Absolute Capture Time can be exposed for diagnostics or forwarded when policy
  allows, but it is never authoritative for `GlobalMediaTime`, deadlines, A/V
  synchronization, or congestion control.
- Unknown extensions are dropped by default. `UnknownExtensionPolicy::Opaque`
  permits byte-preserving pass-through only after URI negotiation and configured
  size validation.
- All authenticated ingress extensions remain available through lazy URI-based
  accessors even when not forwarded.

## Scheduling and congestion control

The public API exposes only semantic policy, allocation, output, and snapshots.
SCReAM, TWCC, RFC 8888 report parsing, queue targets, window gains, pacing factors,
probe clusters, and histories remain private.

The fixed design is:

- one connection-level SCReAM-v2-derived RTP controller;
- one private PulseBeam latency governor that may only tighten the core's native
  queue-delay target through an upper ceiling;
- one weighted max-min allocator over sender media-payload demand;
- one transport-byte pacer and deadline-aware scheduler;
- ordinary RTP padding on a negotiated media or RTX SSRC for pre-media and
  application-limited probing; no synthetic SSRC-zero stream;
- SCTP retaining its own congestion control while its bounded transport load is
  coordinated once with the connection scheduler and media allocator;
- protocol control deliverable under media/DataChannel load;
- padding always lowest value.

The SFU receives governed aggregate and per-sender media-payload allocations and
continues to choose sources and layers. See [`congestion-control.md`](congestion-control.md)
for exact units, constants, state transitions, and acceptance tests.


## DataChannels

DataChannels support local and remote DCEP opening, externally negotiated
channels, ordered and unordered delivery, reliability by retransmit count or
lifetime, text and binary message boundaries, buffered-amount backpressure, and
graceful close. No `dcsctp` type is public.

```rust
pub struct DataChannelConfig {
    pub id: Option<DataChannelId>,
    pub label: Arc<str>,
    pub protocol: Arc<str>,
    pub ordered: bool,
    pub reliability: DataReliability,
    pub priority: DataChannelPriority,
    pub negotiated: bool,
}

pub enum DataReliability {
    Reliable,
    MaxRetransmits(u16),
    MaxLifetime(Duration),
}

#[repr(transparent)]
pub struct DataChannelPriority(NonZeroU16);

impl DataChannelPriority {
    pub const VERY_LOW: Self = Self::new_const(128);
    pub const LOW: Self = Self::new_const(256);
    pub const MEDIUM: Self = Self::new_const(512);
    pub const HIGH: Self = Self::new_const(1024);

    pub fn new(weight: u16) -> Result<Self, DataChannelConfigError>;
    pub const fn weight(self) -> u16;
}

pub enum DataMessage {
    Text(Bytes),
    Binary(Bytes),
}
```

`DataChannelPriority` is the 16-bit SCTP weighted-fair-queueing weight used by the
WebRTC DataChannel profile. It is intentionally a different type and scale from
`MediaPriority`. Higher weight receives proportionally more SCTP service while
backlogged. Message interleaving is used when negotiated; without it, the
scheduler cannot preempt an already fragmented non-interleaved message and
statistics expose the resulting head-of-line delay.

SCTP keeps its own acknowledgment, retransmission, and congestion window. SCTP
packets never enter RTP/TWCC/RFC-8888 sent history and never count as SCReAM
bytes-in-flight. The connection observes bounded SCTP output and acknowledgment
rates, reserves that transport service once when deriving media capacity, and
uses one work-conserving top-level scheduler so DataChannels cannot bypass
protocol service or indefinitely starve media.

Outbound pressure returns `CommandError::WouldBlock` before taking the message.
An oversized authenticated inbound message closes the channel where SCTP/WebRTC
semantics permit rather than the peer connection. Malformed unauthenticated
traffic is dropped and counted.


## Error and isolation contract

- Malformed, unauthenticated, replayed, unknown-tuple, and SRTP-authentication
  failures are dropped with bounded cumulative counters and do not create state.
- Authenticated violations are isolated to their RTP stream or DataChannel where
  protocol semantics permit.
- Unknown, duplicate, stale, or reordered feedback cannot acknowledge bytes more
  than once and cannot allocate unbounded state.
- Dynamic-encoding overflow drops the candidate encoding without closing the
  connection.
- Media/DataChannel queue overflow returns `WouldBlock` or sheds media according
  to deadline policy; it never allocates beyond configured bounds.
- Only unrecoverable authenticated transport state, mandatory resource exhaustion
  required to preserve protocol correctness, cryptographic failure, or timeout is
  terminal. Caller clock regressions are clamped and diagnosed rather than made
  terminal.
- Public errors identify the semantic operation and reason. They do not expose
  `str0m`, `dcsctp`, SCReAM, TWCC, or RTCP implementation types.

## Validation boundary

All implementation checks, tests, fixtures, benchmarks, and browser harnesses for
this project live under `crates/pulsebeam-rtc`. Workspace-wide tests are outside
this implementation project.

Acceptance requires:

- compile-time API tests proving only the documented types cross the crate
  boundary;
- deterministic tests for negotiation, ICE/DTLS/SRTP/SCTP, RTP/RTCP continuity,
  clock normalization, source switching, frame metadata, extension rewriting,
  close behavior, and every resource bound;
- deterministic network simulation for loss, delay, reordering, VBR, pauses,
  probing, feedback loss, malformed traffic, and overload;
- property tests for global-time wire round trips, non-regressing clock mappings,
  no double acknowledgment, bounded histories, allocation conservation, and
  policy monotonicity;
- parameterized many-connection/many-stream benchmarks with one wakeup per
  connection and no global scans;
- live pinned Chrome and Firefox sessions proving offer/answer, UDP and fallback
  ICE-TCP, TWCC, RTP/RTX, pre-media padding on negotiated SSRCs, independent
  playout-delay updates, priority reallocation, source switching, pause/resume,
  VBR forwarding, DataChannels, and graceful close.

Stored SDP cannot prove runtime interoperability. Differential checks against
`str0m`, Ericsson SCReAM, or libwebrtc are component evidence only and never
replace the deterministic contract or live-browser evidence.


## Primary references

- [SCReAM v2, pinned draft revision 01](https://datatracker.ietf.org/doc/html/draft-ietf-ccwg-rfc8298bis-screamv2-01)
- [RFC 3550: RTP and RTCP Sender Reports](https://www.rfc-editor.org/rfc/rfc3550.html)
- [RFC 4571: RTP/RTCP framing over connection-oriented transport](https://www.rfc-editor.org/rfc/rfc4571.html)
- [RFC 6544: TCP candidates with ICE](https://www.rfc-editor.org/rfc/rfc6544.html)
- [RFC 8831: WebRTC DataChannels](https://www.rfc-editor.org/rfc/rfc8831.html)
- [RFC 8832: WebRTC DataChannel establishment](https://www.rfc-editor.org/rfc/rfc8832.html)
- [RFC 8260: SCTP stream schedulers and message interleaving](https://www.rfc-editor.org/rfc/rfc8260.html)
- [RFC 8835: WebRTC media transport priority](https://www.rfc-editor.org/rfc/rfc8835.html)
- [RFC 8888: RTP congestion-control feedback](https://www.rfc-editor.org/rfc/rfc8888.html)
- [libwebrtc transport-wide congestion-control extension](https://webrtc.googlesource.com/src/+/refs/heads/main/docs/native-code/rtp-hdrext/transport-wide-cc-02/README.md)
- [libwebrtc playout-delay extension](https://webrtc.googlesource.com/src/+/refs/heads/main/docs/native-code/rtp-hdrext/playout-delay/README.md)
- [libwebrtc Absolute Capture Time extension](https://webrtc.googlesource.com/src/+/refs/heads/main/docs/native-code/rtp-hdrext/abs-capture-time/README.md)
