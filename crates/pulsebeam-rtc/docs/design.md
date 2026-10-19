# `pulsebeam-rtc` design

Status: normative implementation contract for `pulsebeam-rtc` version one.
Production-server integration is separate; runtime responsibilities below are
requirements on a caller integrating this facade, not claims about the current
`pulsebeam` server.

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
| Client acceptance | Pinned libwebrtc `0.6.2` in controlled simulation; stored SDP only parser evidence | Infer browser/Pion compatibility from fixtures or native acceptance | Required media, TWCC, and DataChannel delivery must cross the actual native engine. |

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
    pub inbound_media: Arc<[InboundMediaInfo]>,
    pub senders: Arc<[SenderInfo]>,
}

pub struct InboundMediaInfo {
    pub sender_index: u32,
    pub kind: MediaKind,
    pub mid: Arc<str>,
    pub rids: Arc<[Arc<str>]>,
}

pub struct SenderInfo {
    pub id: SenderId,
    pub receiver_index: u32,
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

`SessionInfo` is immutable. `sender_index` and `receiver_index` are the remote
endpoint's zero-based SDP media-section positions, including non-RTP sections in
the count. They resolve signaling coordinates to inbound media and stable
`SenderId` resources without making MID public signaling identity. Payload types,
SSRCs, ICE credentials, fingerprints, RTCP parser values, and extension wire IDs
remain connection internals.

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
one complete RFC 4571 frame, including its two-byte frame length. Before the first
frame, the caller registers its accepted stream and exact local/remote tuple with
`AdmitIceTcpFlow`. Every frame MUST repeat that tuple. Admission does not
authenticate the peer: a new incarnation must pass ICE authentication before it
can carry DTLS or media, even when its tuple was previously nominated.

An `IceTcpFlowId` identifies one stream incarnation and MUST never be reused by
the caller. On stream close, the caller issues `RetireIceTcpFlow` before admitting
another incarnation of the same tuple. At most 64 streams are active per
connection. Admission returns `WouldBlock` at this fixed bound, and retirement
reclaims capacity without retaining lifetime tombstones. Unknown or retired flow
input returns `UnknownIceTcpFlow`. Conflicting IDs/tuples or a local address that
is not a passive TCP candidate return `InvalidIceTcpFlow`; repeating an identical
active admission or retiring an inactive ID is idempotent.

Retirement removes pending authentication observations, queued path-bound records
and DTLS input. A retired selected path becomes unavailable under a fresh path
epoch. Protected records are discarded, never retargeted or reconstructed; crypto
state and committed transport identity are not rewound. Established DTLS/SRTP
associations survive a newly authenticated incarnation of their nominated tuple.
Already returned transmits remain irrevocable. Retirement remains available during
graceful close. The caller owns stream reassembly before `receive`, stream writes
after `poll`, and fencing closed stream events before connection delivery.

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
    AdmitIceTcpFlow {
        flow: IceTcpFlowId,
        local: SocketAddr,
        remote: SocketAddr,
    },
    RetireIceTcpFlow {
        flow: IceTcpFlowId,
    },
    SetSenderPolicy {
        sender: SenderId,
        policy: SenderPolicy,
    },
    SetSenderDemand {
        sender: SenderId,
        demand: SenderDemand,
    },
    SendMedia {
        sender: SenderId,
        media: ForwardedMedia,
    },
    RecoverSenderMedia {
        sender: SenderId,
    },
    DiscardSenderMedia {
        sender: SenderId,
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
congestion or deadline shedding. An intrinsically expired packet returns typed
`MediaExpired`, which callers discard rather than retry. Temporary pressure
returns typed `WouldBlock` before taking
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
    pub local_candidates: Vec<LocalCandidate>,
    pub local_ice_credentials: Option<LocalIceCredentials>,
    pub limits: ConnectionLimits,
    pub default_audio_policy: SenderPolicy,
    pub default_video_policy: SenderPolicy,
    pub unknown_extension_policy: UnknownExtensionPolicy,
}

pub enum LocalCandidate {
    Udp(SocketAddr),
    TcpPassive(SocketAddr),
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

`LocalIceCredentials::new(ufrag, password)` accepts opaque caller-owned ICE
credentials. RTC MUST use them unchanged in the SDP answer and ICE agent, without
interpreting routing hints. Ufrag length is 4..=256 bytes, password length is
22..=256 bytes, and both contain only RFC 8445 ICE characters (ASCII letters,
digits, `+`, `/`). Invalid values return `AcceptError::InvalidConfiguration`.
The password is omitted from Debug output. Callers MUST generate a fresh password
from cryptographic entropy for each new connection. `None` retains RTC-generated
credentials derived from the supplied connection entropy. Supplying ICE
credentials does not change the per-connection DTLS identity or key isolation.

```rust
pub struct LocalIceCredentials { /* private */ }

impl LocalIceCredentials {
    pub fn new(ufrag: impl Into<String>, password: impl Into<String>)
        -> Result<Self, AcceptError>;
    pub fn ufrag(&self) -> &str;
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

A configured value above a hard maximum is rejected at construction. Candidate
configuration is caller-owned because the caller also owns the corresponding UDP
sockets and passive TCP listeners. Structural validation permits the default
empty list, accepts at most 16 component-1 host candidates, and rejects duplicate
transport/address pairs, zero ports, wildcard or non-unicast addresses, and IPv6
addresses with flow information or a scope ID. UDP and passive TCP candidates at
the same address are distinct. `Connection::accept` requires at least one
candidate and advertises the complete validated list without interface discovery,
STUN, TURN, or truncation.

Internal
packet counts, feedback histories, timers, and queue horizons have additional
fixed bounds in the congestion-control contract and are not public knobs.

## Events

```rust
#[non_exhaustive]
pub enum Event {
    Connected,
    IceAuthenticated(AuthenticatedFlow),
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
        reason: KeyframeRequestReason,
    },
    AllocationChanged(AllocationSnapshot),
    DataChannel(DataChannelEvent),
    Warning(ConnectionWarning),
}
```

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthenticatedFlow {
    Udp { local: SocketAddr, remote: SocketAddr },
    IceTcp { flow: IceTcpFlowId, local: SocketAddr, remote: SocketAddr },
}
```

`IceAuthenticated` reports a tuple from an accepted ICE Binding Request whose
USERNAME and MESSAGE-INTEGRITY RTC validated against the immutable session
credentials. It is not a nomination, selected-path, DTLS-connected, or media
notification. A routing hint alone, invalid credentials, a malformed packet, or
an accepted success/error response MUST NOT produce this observation. Success
and error response validation authenticates a transaction, not its source tuple.
The event may report a new rebinding tuple before nomination and carries the
caller-owned TCP flow when applicable. Pending identical observations coalesce;
a separate 64-entry bound retains the newest observations under pressure, without
exhausting protocol/media event capacity. Later valid Binding Requests can repeat
an observation. Steering is an optional optimization: the caller MUST remain
correct without receiving or applying a pinning observation.

Events are semantic and bounded. There are no per-packet congestion events.
Repeated malformed input, unknown feedback, drops, and controller diagnostics are
counters in `StatsSnapshot`; `Warning` is reserved for actionable state changes
that a caller may need to surface.

`AllocationSnapshot.media_budget` is the controller-governed RTP payload selection
ceiling, capped by bounded potential demand. It is not measured spare bandwidth,
physical path capacity, or a reservation. `total` is the sum of the selected-demand
sender allocations, which can be lower or zero. A fixed selected encoding is not
unconditionally discounted by its latency profile; if selected aggregate demand
fits the current selection ceiling, every selected source rate is fully funded. The snapshot
includes every negotiated sender, including zero allocations. Budget-only changes
are observable even when allocations are unchanged. Pending updates coalesce into
one latest-state snapshot; downward changes are not held for confirmation.
`StatsSnapshot.connection.media_budget` reports the same current ceiling for
startup and coherent inspection. Source/layer quality hysteresis remains SFU-owned.

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
    InvalidIceTcpFlow,
    InvalidFrameMetadata,
    MediaExpired,
    FrameInterrupted,
    DependencyChainInterrupted,
    PlayoutResetRequiresNewSender(SenderId),
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

- connection state, negotiated packet-feedback kind, and close reason;
- caller clock regressions, RTP bytes in flight, target media-payload bitrate,
  pacing bitrate, and queued media/data bytes;
- transmitted RTP, RTCP, SCTP, other protocol, and padding bytes;
- dropped network inputs and unknown, duplicate, stale, and wrong-path feedback.

Required per-sender observations include:

- sender identifier and policy, current allocation, queued packets/payload bytes,
  and transmitted packet/payload totals.

Required per-encoding observations are the encoding identifier and media kind,
received packet count, and retirement state. Required per-DataChannel
observations are identifier, priority, reliability, buffered amount, and sent
and received message counts.

Private controller names and mutable internal objects are not returned.


## Session and interoperability contract

- The session is one immutable ICE-lite remote-offer/local-answer exchange.
- The caller supplies every advertised local host candidate and retains ownership
  of its matching socket or listener. Negotiation converts that ordered set once
  and retains it with the immutable ICE credentials for answer construction and
  later transport initialization.
- BUNDLE, RTCP mux, inline candidates, all SDP media directions, UDP, and passive
  ICE-TCP over IPv4 and IPv6 are supported.
- Local server-reflexive and relay gathering, active or simultaneous-open TCP,
  interface discovery, STUN discovery, and a TURN client are outside the local
  candidate profile. Remote relay candidates remain supported.
- Trickle ICE, ICE restart, media renegotiation, and a TURN client are outside this
  boundary. A changed session creates a new connection. Client relay candidates
  remain usable.
- `Connection::accept` creates a fresh ephemeral DTLS identity from the supplied
  cryptographic entropy. Its private key is never exported or reused.
- Codecs and RTP header extensions are negotiated from immutable
  `SessionCapabilities`. Syntactically valid optional extension mappings whose
  direction is incompatible with active media are omitted from the answer.
  This narrow compatibility rule accommodates Firefox's receive-only
  playout-delay mapping on send-only media without advertising unusable
  capability. MID and TWCC direction checks, extension syntax, IDs, duplicate
  mappings and the inline-candidate requirement remain strict.
- Core forwarding does not require codec-payload parsing and MUST carry opaque or
  SFrame-protected media.
- Every accepted outbound RTP session, including audio-only RTP, MUST negotiate
  one supported packet-feedback mode: transport-wide congestion-control feedback
  or RFC 8888 congestion-control feedback. Absence is a specific
  `AcceptError::MissingPacketFeedback`; there is no REMB or fixed-rate fallback.
- A valid SCTP application-only session has no packet-feedback mode and no RTP
  senders. It does not enable a fixed-rate RTP fallback or RTP sent accounting.
- The selected packet-feedback mode is immutable and applies to every bundled
  outbound RTP sender. TWCC and RFC 8888 are not run concurrently for the same
  RTP path.
- Compatibility means compatibility with the negotiated PulseBeam WebRTC
  profile, not every theoretically standards-compliant SDP. Client acceptance
  uses only pinned libwebrtc `0.6.2` in controlled execution. Stored SDP is generic
  parser regression evidence only; browser and Pion compatibility are not claimed.

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
    pub fn view(&self) -> Option<MediaPacketView<'_>>;
    pub fn extension(&self, uri: &str) -> Option<&[u8]>;
    pub fn dependency_descriptor_context(&self) -> Option<(&[u8], u32)>;
    pub fn to_transit(&self) -> MediaPacket;
    pub fn materialize(&self) -> Result<MaterializedMediaPacket, MediaTransitError>;
}

pub struct MediaPacketView<'a> {
    pub ssrc: u32,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub header_len: usize,
    pub payload: &'a [u8],
}

pub struct MaterializedMediaPacket { /* owned, no shared packet refcounts */ }
impl MaterializedMediaPacket {
    pub fn from_bytes(bytes: Box<[u8]>) -> Result<Self, MediaTransitError>;
    pub fn as_bytes(&self) -> &[u8];
    pub fn into_bytes(self) -> Box<[u8]>;
    pub fn into_packet(self) -> Result<MediaPacket, MediaTransitError>;
}

pub struct ForwardedMedia {
    pub packet: MediaPacket,
    pub frame: FrameMetadata,
}
```

`MediaPacket.global_media_at` is immutable. A relay MUST serialize and preserve it
exactly; an egress connection never reconstructs or rebases a transit packet's
value. `MaterializedMediaPacket` is a fully owned representation for shard/node
transit. Cloning it deep-copies all storage, and restoration creates fresh local
packet reference-count state. Its versioned wire encoding preserves the complete
canonical RTP plaintext, URI-based extension views, exact eight-byte
`GlobalMediaTime`, and private clock-quality/uncertainty classification. No local
`Instant` or reconstructed compact playout time crosses this boundary.

Packet wire version one is: `PBRP` magic (4 bytes), version (1 byte), private
clock-quality tag (1 byte), extension count (u16), global microseconds (u64), RTP
length (u32), zero reserved field (u32), then extension records and canonical RTP.
Integers are big-endian. Each extension record contains URI byte length, range
start and range end as u32 values followed by UTF-8 URI bytes. Ranges address the
canonical RTP, not the materialized wrapper. Bounds are 512 KiB total, 65,535 RTP
bytes and 256 extension records. Decoding MUST reject unknown versions/tags,
nonzero reserved fields, truncation/trailing bytes, invalid RTP, duplicate or
invalid URIs, and ranges that are not actual RTP extension values. Oversized
materialization returns a typed resource error, not an unbounded buffer.

Authenticated DD ingress retains the last source template-definition descriptor
and current active-target mask as immutable packet metadata. The passive
`dependency_descriptor_context` accessor returns those source facts, not a frame
to replay, a decoded payload, or a local transport identity. It permits cold
source joins even when the current source descriptor omits its definition.
Local clones MAY share these immutable bytes; both transit paths MUST deep-copy
them and preserve canonical packet bytes and global media time unchanged.

Packets with this context use wire version two. After the extension records and
before canonical RTP, it adds definition length (u16), current active-target
mask (u32), then the definition bytes. The definition MUST be 3..=255 bytes,
contain a valid attached structure at frame start, agree with the current
packet's DD structure, and permit the supplied active-target mask. An explicit
current-packet mask MUST agree with the context. Version-one packets remain
supported and contain no context. Neither version fabricates absent source DD
facts from generic `FrameMetadata`.

This codec validates representation, not source authentication. It is a trusted
SFU relay boundary, not an alternative to `Connection::receive` or ICE/SRTP
authentication. The relay remains responsible for route/epoch/authentication
framing and for serializing its separately owned `FrameMetadata` facts.
`ForwardedMedia::to_transit` deep-copies packet state and known dependency IDs;
unknown dependencies remain unknown. A relay must not replace unknown facts
with invented independent-frame metadata.

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
    pub decode_target_mask: Option<u32>,
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

`decode_target_mask` is the SFU's semantic output selection, not a mutation of
source packet bytes or source descriptor context. `None` preserves all source
active targets. `Some(mask)` selects a nonzero subset of the source definition's
decode-target indices; zero and out-of-definition bits are invalid frame metadata.
When outbound DD is negotiated, selection requires a valid source DD and the
emitted active-target mask is the intersection of selection and current source
active targets. A source may deactivate every selected target, yielding an empty
emitted mask without fabricating activity. Without negotiated outbound DD, a
nonzero mask remains caller semantic metadata and generates no wire extension.
`FrameMetadata::to_transit` preserves selection exactly.

For every packet in one frame:

- `id`, `random_access`, `discardable`, `dependencies`, and `decode_target_mask`
  MUST be identical;
- `global_media_at`, canonical source SSRC and RTP timestamp MUST be identical;
- a one-packet frame uses `Complete`;
- a multi-packet frame uses one `Start`, zero or more `Middle`, and one `End`;
- dependencies MUST NOT contain the current frame ID;
- metadata inconsistency is `CommandError::InvalidFrameMetadata` and rejects the
  packet without mutating RTP continuity.

A `Start` or `Complete` for a different frame while an admitted frame is still
open returns `CommandError::FrameInterrupted`, even for random-access media.
This does not accept the new packet or automatically discard prior work. It lets
the caller distinguish interrupted source framing from ordinary queue pressure
and malformed continuation metadata. A `Middle` or `End` without an admitted
open frame also returns `FrameInterrupted`: its starting fragment is missing.
A dependent frame with an invalidated dependency chain returns
`DependencyChainInterrupted` when no useful independent packet is queued for that
sender. It requires recovery, not retrying the same dependent packet indefinitely.
While a useful independent packet is queued, the dependent frame returns
`WouldBlock` until that recovery packet commits or expires.
For an otherwise matching `Middle` or
`End`, canonical source RTP sequence MUST be the admitted predecessor plus one,
including 16-bit wrap. A missing, duplicated or reordered source fragment returns
`FrameInterrupted` without admitting or transmitting that continuation. This
prevents renumbering a missing-middle frame into contiguous output with a false
End marker. Whole-frame selection and shedding may skip arbitrary source sequence
numbers between frames. Within the same publication the caller may issue
`RecoverSenderMedia` to cancel unprepared media and source-dependent continuity
while retaining committed repair history, then resume at a fresh
independent `Start` or `Complete`, or request one from the source. An independent
`Middle` or `End` cannot restart a discarded frame. Committed RTP/feedback identity
and the global-time fence remain intact. A duplicate start for the same frame,
a queued duplicate frame, and mismatched continuation facts remain invalid
metadata, not interruption. Expiry is not a recovery guarantee after a start has
already been transmitted and its queue is empty.

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

```rust
pub struct EncodingInfo {
    pub id: EncodingId,
    pub sender_index: u32,
    pub kind: MediaKind,
    pub rid: Option<Arc<str>>,
}
```

- Negotiated inbound encodings are admitted and receive stable `EncodingId`
  values.
- Every authenticated encoding, including a negotiated SSRC or RID, emits
  `EncodingDiscovered` before its first `Media` event. Discovery includes the
  immutable inbound `sender_index`, media kind, and optional RID so the SFU can
  associate the encoding with a publication and simulcast layer without parsing
  SDP or accessing protocol internals.
- Bundled media sections may share a payload type. Ingress resolves the negotiated
  MID first, then the negotiated or previously authenticated SSRC association,
  and uses payload type alone only when it identifies one inbound section.
  Unknown or malformed MID, conflicting MID/SSRC associations, and ambiguous
  packets are dropped without creating an encoding. A retired association cannot
  be resurrected by later packets.
- Unsignaled encodings consume the configured connection-wide budget. Overflow
  is dropped and counted without closing the connection.
- Inactivity never retires a paused encoding. RTCP BYE or
  `Command::RetireEncoding` does.
- Malformed or unauthenticated packets never create an encoding.
- Primary RTP and incoming RTX share a 256-packet source sequence window.
  Delivery remains cut-through and can be reordered; duplicates and packets
  older than the window are dropped. RTX uses the negotiated payload `apt` and
  FID SSRC pair, or a repaired RID associated with an existing primary encoding.
  Its original sequence number, primary payload type and primary SSRC are
  restored before canonical source delivery. An unknown repair association
  cannot create a separate primary encoding.
- When generic NACK is negotiated, gaps wait 10 ms for reordering, then receive
  at most five NACK attempts spaced 50 ms apart. Missing-packet state is bounded
  to the source window. Receiver reports run every second and include loss,
  extended sequence progression, jitter and the last Sender Report relationship.
- Publisher congestion feedback is negotiated independently from subscriber
  feedback. TWCC covers one bundled transport sequence space, including RTX and
  padding. RFC 8888 reports original and repair SSRCs separately, retaining
  receiver arrival offsets and observed ECN marks. Authenticated wire receipt is
  recorded before application queue admission and before RTX normalization.
- Publisher packet feedback runs every 50 ms with at most 512 retained wire
  receipts and 256 statuses per report. Reports preserve sequence wrap,
  reordering and late arrivals. Overload forgets old receipts rather than growing
  history or generating unbounded reports; RFC 8888 holds over-range arrival
  offsets explicitly. SRTP replay protection is not reset to admit same-SSRC
  sequence restarts.
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

### Sender-scoped recovery and discard

For interrupted-frame recovery or a source/layer change within the same
publication, the SFU may issue `RecoverSenderMedia`. It clears that sender's
unprepared queued media and open/started frame state and invalidates dependency
continuity, but retains already-committed RTP originals and pending repairs.
Repair eligibility, pacing, send-window, byte and age bounds remain unchanged.
Keeping repair history permits late NACKs to complete frames committed before
recovery. Recovery never refunds spent payload admission or transport service.

Before pausing, removing or replacing a publication/assignment, the SFU MUST use
`DiscardSenderMedia`, not recovery, to fence old repair payloads from a replacement
occupant. Both operations preserve the identity and committed state described
below and leave other senders unaffected.

`DiscardSenderMedia` removes that sender's unprepared queued media,
open/started frame state, queued repairs and retained RTX payloads in bounded
connection-local work. Other senders are unaffected. Dependency continuity is
invalidated; subsequent random-access or known-independent media must establish
a fresh chain. Unknown dependencies must not be invented as empty dependencies
to bypass that requirement.

Discard preserves negotiated identity, primary/RTX SSRCs, sequence spaces,
global-to-RTP mapping, committed media-time fence, counters, playout confirmation
epochs, and connection-wide TWCC/sent-history/controller/pacer state. It may
release the admitted-time fence of discarded work only back to the latest
committed media time. It never makes older cached media eligible to rewind the
receiver timeline.

Each audio sender initially arms a talk-spurt marker. Discard rearms it for a
replacement or resumed source. Preparing audio RTP preserves the source marker
and adds the pending marker; only committing an original transmit consumes the
pending marker. Queue admission, unprepared queue drops and rejected discard do
not consume it. Repairs retain the original wire marker. Video markers continue
to follow source frame boundaries. The canonical source packet is never changed.

Protection consumes the SRTP index. Discard returns `InvalidState` without
mutation if any egress preparation is pending; normal serialized public
`poll` prepares and commits before returning. Already emitted transmits remain
irrevocable and must have received the caller's immediate submission attempt
before any discard command. This is not network rollback and does not cancel
committed traffic or its feedback accounting.

## Sender policy

Every negotiated outbound sender has one mutable semantic policy. Configuration
provides the initial policy, so no sender starts undefined.

```rust
pub struct SenderPolicy {
    pub playout_delay: Option<PlayoutDelay>,
    pub priority: MediaPriority,
    pub desired_bitrate: MediaPayloadBitrate,
}

#[repr(transparent)]
pub struct MediaPayloadBitrate(u64); // bits per second of RTP media payload

#[repr(transparent)]
pub struct MediaPriority(NonZeroU16);

pub struct PlayoutDelay {
    min_millis: u16,
    max_millis: u16,
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

Explicit bounds are in `0..=40_950 ms`, with minimum no greater than maximum.
Construction rejects out-of-range values and does not clamp. Local latency policy
retains the millisecond bounds; RTP extension encoding rounds each bound to the
nearest 10 ms. The exact constructor additionally rejects non-tick-aligned inputs.
The server normalizes client inputs to 0–2000 ms and raises maximum to minimum
before constructing explicit policy.

```rust
impl PlayoutDelay {
    pub fn from_ticks(min: u16, max: u16) -> Result<Self, PolicyError>;
    pub fn from_millis(min: u64, max: u64) -> Result<Self, PolicyError>;
    pub fn from_millis_exact(min: u64, max: u64) -> Result<Self, PolicyError>;
    pub fn min(self) -> Duration;
    pub fn max(self) -> Duration;
}
```

`None` is browser-managed default playout, not an explicit extension value.
Configuration defaults to `None`. `Some(PlayoutDelay::ZERO)` explicitly requests
rendering as soon as practical, not a literal zero network or render deadline.
Both modes retain bounded ASAP fresh-body admission. Unspecified playout uses
bounded decoder-repair history distinct from explicit `0/0`, as defined in
[congestion control](congestion-control.md). It never grants unlimited queueing or
artificial transport capacity.

When the playout-delay RTP extension is negotiated for outbound use and policy
is explicit, the connection writes the current value for that sender. After a change it repeats the value until an RTCP
receiver report proves, using wrap-aware outbound RTP sequence progression, that
a packet carrying the new value was received. TWCC acknowledgment alone is not
used as proof of extension delivery. When the extension is absent, the local
policy still governs admission and statistics mark it `NotSignaled`. Unspecified
policy never writes a delay extension, and statistics mark it `Unspecified`.
Explicit delivery status is `Pending` until an authenticated SR/RR report block
covers a committed primary-SSRC packet carrying the current value, then
`Confirmed`. RTX-SSRC reports and reports beyond committed progression do not
confirm it. Changing an explicit value resets confirmation without resetting RTP
or congestion state. Returning a sender that has committed any explicit delay
extension to unspecified mode returns `PlayoutResetRequiresNewSender`; the SFU
must use a fresh receiver or reconnect because omission cannot reset browser
state. An explicit policy that has never been signaled may return to defaults.

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
When local video expiry invalidates dependency continuity, the connection emits
one coalesced `KeyframeRequested` with `LocalRecovery` per affected sender. Pending
recovery is exposed before returning `Idle`; callers abandon deferred dependent
writes and select fresh independent media. Authenticated peer PLI/FIR feedback
emits `KeyframeRequested` with `Peer`, which requests future independent media
without abandoning useful current delivery. Local recovery takes precedence when
both reasons are pending. Audio expiry does not request a keyframe.

### Desired bitrate

`desired_bitrate` is the RTP media-payload rate the SFU could use if given the
capacity, after choosing a source or layer. Zero rejects new media admission.
It is not a network estimate and does not include RTP, SRTP, UDP/TCP, RTCP, or
DataChannel overhead.

`Command::SetSenderDemand` atomically declares selected and potential payload
rates using `SenderDemand::new(selected, potential)`; potential below selected
is rejected. Selected updates `SenderPolicy.desired_bitrate` and remains the
actual selected allocation/service demand. Potential is the bounded media rate the SFU
could actually use from available source/layer choices, not a bandwidth estimate,
reservation, or permission to transmit that rate. It supplies the aggregate
application maximum to the unchanged SCReAM adapter and bounded probe interest.
A paused, unfunded stream can declare zero selected and positive potential.
Hiding or withdrawing interest declares both zero.

Until an explicit demand declaration, potential follows selected for existing
policy callers. Later `SetSenderPolicy` changes retain explicit potential,
raised to at least the newly selected rate. Statistics report the effective
`SenderDemand`. Priority-first source/layer selection remains SFU-owned;
potential never participates in weighted media allocation or grants body-media
admission. Ordinary zero-to-positive application-demand updates preserve the
native controller state on an unchanged path; they do not reset congestion
history or manufacture available capacity.

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
  generated or rewritten by the connection. DD rewriting uses actual source
  templates, DTIs, chains and resolutions, never payload parsing. It remaps frame
  and active-chain references only to committed outbound frames in a bounded
  exact source history. Missing required predecessors fail closed and request
  fresh random access; zero is not a substitute for a shed predecessor. Chains
  protecting no emitted active target carry zero differences because they do
  not declare an output dependency. Source active-mask fields are never copied
  over the SFU's semantic selection. A cold independent
  frame carries its real source definition without replaying older media. RTX
  preserves the original outbound DD value. DD-capable senders use a stable
  two-byte extension format, and queue estimates account for forced definitions.
- Known endpoint-independent values such as audio level and video orientation can
  be forwarded after URI-based remapping.
- Absolute Capture Time can be exposed for diagnostics or forwarded when policy
  allows, but it is never authoritative for `GlobalMediaTime`, deadlines, A/V
  synchronization, or congestion control.
- Unknown extensions are dropped by default. `UnknownExtensionPolicy::Opaque`
  permits byte-preserving pass-through only after URI negotiation and RTP's
  255-byte per-value bound. Audio level and video orientation require one byte;
  Absolute Capture Time requires eight or sixteen bytes. Invalid values are
  omitted rather than emitted as malformed extensions. A sender with an opaque
  value or an extension ID outside the one-byte range always uses the two-byte
  wire format, even when that packet omits the optional value. Optional values
  must not make a stream switch extension formats without negotiated mixing.
- All authenticated negotiated ingress extensions remain available through lazy
  URI-based accessors even when not forwarded.
- RTX retains the original canonical outbound RTP within the retransmission byte
  budget. Repairs preserve endpoint-independent extension values and the original
  marker, but generate new transport-owned RTX identity, MID and feedback state.
  Unretained originals do not create repair requests; eviction retires queued
  requests for the evicted original.

## Scheduling and congestion control

The public API exposes only semantic policy, allocation, output, and snapshots.
SCReAM, TWCC, RFC 8888 report parsing, queue targets, window gains, pacing factors,
probe clusters, and histories remain private.

The fixed design is:

- one connection-level SCReAM-v2-derived RTP controller;
- one private PulseBeam latency governor for admission, allocation, pacer horizons,
  retransmission, and probing outside the SCReAM core;
- one weighted max-min allocator over sender media-payload demand;
- one transport-byte pacer and deadline-aware scheduler;
- ordinary RTP padding on a negotiated media or RTX SSRC for pre-media and
  application-limited probing; no synthetic SSRC-zero stream;
- SCTP retaining its own congestion control and bounded buffering, coordinated
  through eligible connection-level packet service without inferred ACK-rate reservations;
- protocol control deliverable under media/DataChannel load;
- padding always lowest value.

The SFU receives the RTP payload selection budget separately from demand-capped
aggregate and per-sender allocations and continues to choose sources and layers. See [`congestion-control.md`](congestion-control.md)
for exact units, constants, state transitions, and acceptance tests.


## DataChannels

DataChannels support local and remote DCEP opening, externally negotiated
channels, ordered and unordered delivery, reliability by retransmit count or
lifetime, text and binary message boundaries, buffered-amount backpressure, and
graceful close. No `dcsctp` type is public.

`DataChannelEvent::Opened` carries the assigned channel ID and a complete
`DataChannelConfig` snapshot for local, remote DCEP, and externally negotiated
channels. The snapshot's `id` is always the assigned ID, not an unresolved opening
request. Labels, protocols, ordering, reliability, and priority remain available
for application classification without consulting private SCTP state.

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
bytes-in-flight. The connection records committed SCTP transport bytes as sends,
not acknowledged delivery or spare path bandwidth. Bounded protocol service and
alternating eligible RTP/SCTP packet service coordinate the two controllers without
an inferred combined delivered-rate estimate or a fixed-rate reservation. Ready
SCTP output takes precedence over padding. DataChannels cannot bypass protocol
service or indefinitely starve eligible media; blocked SCTP does not stall RTP.

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

Crate-local deterministic tests and the controlled native harness live under
`crates/pulsebeam-rtc`. Private controller assertions, including the fixed SCReAM
scenario matrix, run once in the unit target. Integration tests protect the
public `Connection` boundary, and controlled native peers protect interoperability.
Web and React retain consumer browser coverage. The
[shared development workflow](../../../README.md#development) defines workspace
gates.

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
- actual libwebrtc `pulsebeam-webrtc-sys` `0.6.2`, source commit
  `f87f99a48e58fb9cd13767ba58e9b7cd937c71a3`, and matching released native
  artifacts provisioned through Bazel on Linux x86_64;
- controlled execution with explicit task pumping, simulated clocks, virtual
  packet delivery, and bounded progress; no production scheduling or live sockets;
- authenticated bidirectional media, actual native TWCC affecting send accounting,
  text/binary DataChannel delivery and media coexistence, lifecycle handling,
  and repeatable same-seed results.

The canonical complete RTC local and CI command is:

```sh
./bazel test --config=sim //crates/pulsebeam-rtc:test --nocache_test_results
```

This includes all unit, property, integration, and native simulation tests.
With built binaries and cached test results disabled, the operational host
elapsed-time target is fifteen seconds per complete invocation, including peer
setup and teardown. Dependency fetching, compilation, lint/format checks,
benchmarks, and other owners are excluded. This target is not a correctness
acceptance threshold or a host-performance benchmark. Virtual-time protocol
deadlines, resource limits, and quantitative modeled-network/controller assertions
remain correctness requirements. Required native scenarios cannot be moved to
optional slow gates.
Missing artifacts or absent required media, feedback, or DataChannel evidence
fail acceptance, never silently skip or fall back to another client.

RFC 8888 retains deterministic client-free negotiation, report generation,
parsing, and accounting evidence; RFC 8888 client interoperability is not claimed. ICE-TCP also has
controlled active-client libwebrtc interoperability at the packet-socket boundary,
including authenticated media, DataChannels, flow retirement, and graceful close.
The adapter has no TCP listener and supplies unframed non-TLS packet payloads;
the fixture translates these to and from RTC's complete RFC 4571 frames. Native
TCP byte-stream fragmentation and passive-client coverage are not claimed.
Other platforms, ICE restart, and renegotiation remain outside this profile.

Run root `./bazel test //:test` for complete workspace acceptance, including
consumer-owned Web and React browser coverage. Shared browser provisioning is
retained in `tools/browser-matrix.json`.

Host-performance comparisons use scaling measurements, separate from correctness
test gates. The parameterized matrix preserves one wakeup per connection and no
global scans. Run the Criterion matrix with
`./bazel run //:benchmark`. It measures timer/poll
scheduling for 1/16/64 connections and 1/8/32 senders, excluding fixture setup and
teardown. This initial-burst workload is not sustained network throughput
evidence. Benchmark reports and baselines are emitted under `target/criterion`.

Stored SDP cannot prove runtime interoperability. Client-free protocol checks
and Ericsson SCReAM comparisons establish narrower RTC component invariants,
not production-server integration or additional client compatibility. Only controlled actual libwebrtc delivery
establishes the native interoperability baseline.


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
