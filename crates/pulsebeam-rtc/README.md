# `pulsebeam-rtc`

The sans-I/O WebRTC transport engine used by the PulseBeam SFU. This contributor
guide covers its transport boundary; source/layer selection and media projection
remain in the server. Focused transport proofs do not establish full native-agent
or simulator qualification.

For a first development check, run the owner suite from the workspace root:

```sh
./bazel test //crates/pulsebeam-rtc:test --test_output=errors
```

See the [connection contract](docs/design.md) for integration obligations and
[congestion control](docs/congestion-control.md) for the private SCReAM profile.
The public Rust definitions in `src/lib.rs`, `src/rtp.rs`, `src/io.rs`, and
`src/session.rs` are the API reference.

## Ownership

RTC owns negotiation, ICE, DTLS, SRTP/SRTCP, RTP/RTCP, SCTP, packet feedback,
congestion control, transport pacing, ordinary repair, timers, and mechanical
resource limits. Each connection is `Send`, not `Sync`, with explicit protocol
progress and owned outputs.

The caller owns sockets and scheduling, routing and bandwidth allocation,
source/layer selection, receiver sequence/timestamp translation, DD/frame
interpretation and rewriting, keyframe caches, source switching, playback
policy, global media normalization, and node serialization. RTC has no source
assignment, frame metadata, media-age admission, or global media timeline.

## Direct connection operations

Accept an initial offer using local candidates, a standard clock anchor, and
fresh cryptographic entropy supplied by the caller. This helper illustrates the
workspace API; it does not create sockets or obtain entropy:

```rust
use std::{net::SocketAddr, time::{Instant, SystemTime}};
use pulsebeam_rtc::{
    AcceptError, AcceptedConnection, ConnectionConfig, ConnectionEntropy, LocalCandidate,
    RtcConnection, SdpOffer,
};

fn accept_peer(
    local: SocketAddr,
    offer: String,
    now: Instant,
    wallclock: SystemTime,
    fresh_entropy: [u8; 32],
) -> Result<AcceptedConnection, AcceptError> {
    RtcConnection::accept(
        ConnectionConfig {
            local_candidates: vec![LocalCandidate::Udp(local)],
            ..ConnectionConfig::default()
        },
        SdpOffer::new(offer),
        now,
        wallclock,
        ConnectionEntropy::new(fresh_entropy),
    )
}
```

Send the returned SDP answer through the application's signaling channel.
Then use the connection directly:

* `receive(now, arrival, input)` accepts a UDP datagram or one complete RFC 4571
  ICE-TCP frame. Preserve actual arrival separately from delayed processing.
* `poll(now)` services protocol work and due timers, returning an event,
  committed transmit, next wakeup, or terminal close. Honor an immediate wakeup
  as runnable work. A caller can yield after a bounded output batch.
* `write_rtp(now, mid, packet)` writes to a negotiated outbound media section.
  `RtpWrite` supplies the extended primary sequence, timestamp, its associated
  monotonic `wallclock`, marker, payload, and negotiated media extension values.
* `open_data_channel`, `send_data`, and `close_data_channel` operate directly on
  channels. Backpressure returns caller-owned work.
* `set_desired_bitrate` sets one optional aggregate demand hint. `stats` and
  `Event::BandwidthEstimate` expose the native media-payload rate estimate.
* `discard_pending_rtp` discards uncommitted originals for a stream without
  resetting transport history or retained repairs. `close` performs bounded
  graceful shutdown; `abort` discards pending work and closes immediately.

## Data channel lifetimes

`data_channel_capacity()` reports the actual negotiated bidirectional SCTP
stream count once connected, not the number of free channels. Native defaults
advertise the full `u16` stream count in each direction without per-stream fixed
storage. The peer can negotiate fewer streams; capacity is the minimum of the
negotiated incoming and outgoing counts. Explicit and automatically selected IDs
must be below that count, including the highest valid ID `u16::MAX - 1` when both
counts are maximal. DCEP allocation still respects its directional parity.
Channel admission uses funded storage, not a separate live-row quota. This
breaking pre-v1 API removes `ConnectionLimits::max_data_channels`; configure
`max_data_runtime_bytes` for the retained channel/config/reset/event/lifecycle
payloads described below. Peer negotiation, SID bounds and reset barriers still
apply, independently of available credit. The public `DEFAULT_MAX_DATA_CHANNELS`
(256) and `HARD_MAX_DATA_CHANNELS` (`u16::MAX`) constants remain only as legacy
compatibility values, not configured or enforced admission ceilings.

`open_data_channel` can reserve a negotiated channel with an explicit ID before
SCTP connects. Channel/configuration storage, reset state and future notices are
funded immediately; no DCEP OPEN is sent. Activation rechecks the peer's actual
stream capacity before `Opened` or message delivery. Notifications progress in
bounded turns. DATA received before COOKIE ACK completes the association stays
in dcSCTP's receive queue until negotiated stream facts are available. The first
inbound DATA then activates its declaration before queuing delivery, preserving
`Opened` before `Message`. Pending declarations cannot send. Cancellation, reset-all and shutdown fence activation. A canceled
or unrepresentable never-opened declaration reports closure/reset failure and
releases its full configuration, retaining only a funded non-reuse guard until
teardown because no reset barrier completed. In-band and automatically assigned
channels still require connected SCTP.

For an exact explicitly identified negotiated configuration, use
`install_data_channel(now, config)` instead of a caller-owned reset/reopen loop.
The returned `DataChannelInstallation` fences both the association and the
installation lifetime. RTC prepays the pending configuration, channel, reset and
notification storage before accepting it, then waits locally for SCTP connection
and the previous SID's complete reset barrier. Rejected admission returns the
unchanged configuration. Pending installations cannot send and do not hold up
other application operations. In-band and automatically assigned channels keep
the direct `open_data_channel` API.

Use ordinary channel events as wakeups and query `data_channel_installation(token)`
before attaching a completion. An `InstallationFailed` event retains the exact
token when a request fails before admission, independently of an older lifetime
still resetting on that SID. `Pending`, `Open { at }` and `Closed` describe that
exact lifetime; `at` is actual RTC activation time. `None` means failed, canceled,
retired or unavailable, including a token from another association. Peer-capacity
rejection, failed reset, reset-all and shutdown fence activation. Progress shares
the existing bounded SCTP service turns, with no spin or pending-map scan while
waiting on a reset. `cancel_data_channel_installation` releases a not-yet-admitted
request or closes the exact installed lifetime. Stale cancellation cannot close a
replacement; `Closed` still does not permit SID reuse. Terminal pending-request
reclamation and its retained failure facts also progress in bounded polling turns.

SDP `max-message-size` is directional: our answer advertises our receive ceiling,
not the offer's value. That ceiling is the larger of the configured application
message limit and the DCEP format maximum: a 12-byte OPEN header plus two
independent `u16` byte-length fields, totaling 131,082 bytes.
Application delivery still enforces its own configured limit. Complete inbound
messages exceeding the advertised ceiling reset only their stream, even with an
empty-message PPID. The native receive window includes this control allowance
even when application limits are zero.
The peer's advertised maximum independently bounds every outbound SCTP user
message, including DCEP. An omitted peer attribute means 64 KiB; zero means no
peer-imposed maximum, not unlimited local storage. A non-negotiated OPEN that
would exceed the peer maximum returns its configuration with `InvalidState`
before changing channel state or consuming its lifetime. Negotiated channels
send no OPEN and can still be admitted under a smaller peer maximum.
Empty text/binary messages consume one SCTP wire byte but zero caller-payload
bytes. See [RFC 8841 section 6.1](https://www.rfc-editor.org/rfc/rfc8841.html#section-6.1),
[section 10.3](https://www.rfc-editor.org/rfc/rfc8841.html#section-10.3), and
[RFC 8831 section 6.6](https://www.rfc-editor.org/rfc/rfc8831.html#section-6.6).
The former combined 64 KiB DCEP codec ceiling is removed. Each label and protocol
still must fit 65,535 UTF-8 bytes, not characters. Large OPENs need sufficient peer
message capacity and funded local configuration/event/reset storage; the format
bound overrides neither. In particular, an omitted peer attribute still prevents
OPENs above 64 KiB. Negotiated channels send no OPEN but retain the per-field
bounds and configuration-storage admission. See
[RFC 8832 section 5.1](https://www.rfc-editor.org/rfc/rfc8832.html#section-5.1).

`ConnectionLimits::max_buffered_data_bytes` provides independent outbound and
inbound payload allowances. Outbound caller payload bytes stay charged until the
native SCTP lifecycle ends. Inbound completed messages charge their retained `Vec`
capacity, including spare capacity, while queued for local delivery. Empty-message
PPIDs retain no payload allocation. Inbound payload exhaustion resets the offending
stream without closing healthy streams. Dequeue, including stale-lifetime drops,
ends that charge; caller-retained deliveries are outside the queue's scope.
Explicit abort discards queued backing even after the association has stopped.
Native socket/reassembly buffers and event/index storage are separate, not covered
by this payload ledger. Channel admission has no additional count ceiling.

The inbound-message and buffered-payload defaults remain 1 MiB and 8 MiB. Their
former 16 MiB and 64 MiB validation ceilings are removed; the public `HARD_MAX_`
constants retain those legacy values for source compatibility, not enforcement.
Instead, one checked native-buffer rule derives the receive window as the larger
of buffered bytes and the advertised receive ceiling plus one SCTP MTU, and the
send buffer as buffered bytes plus the DCEP control allowance. Both resulting options must
fit `u32`, because dcSCTP narrows them into advertised receive/TCB window fields.
Overflow or unrepresentable options return `InvalidConfiguration` before native
socket construction, not a silent clamp. The same rule supplies native receive,
total-send and per-stream-send buffer options. These are wire-width bounds, not
promises that allocating such large messages is practical; the configured
per-message, retained-payload and native queue admissions still apply.

`ConnectionLimits::max_data_runtime_bytes` separately funds local SCTP lifecycle-map
key/value payloads, including records for empty sends. Its default is 64 MiB, with
no fixed record or validation ceiling. Admission checks prospective record storage
before copying caller payloads, sending to the socket or advancing the lifecycle
identity. Insufficient storage returns `WouldBlock` with the caller's message
unchanged. Native lifecycle completion or map cleanup releases the record charge;
channel closure alone does not. This replaces the local 4,096-message ceiling.
The same allowance prepays one reset-map key/value and ready-set member per
admitted channel, before advancing its lifetime or sending DCEP. Closing consumes
that promise without needing free bytes. Both reset directions and the native
receive-FIFO drain barrier must complete before the reservation is released;
failed resets retain it until teardown. Unknown-SID resets reserve their own pair
of payloads and one future reset-outcome notice, even without a local channel
declaration. Duplicates reuse that credit. If an unknown reset barrier cannot
be retained, the association stops rather than permitting unsafe SID reuse.
Explicit abort releases all reset credit, even after stop.
Channel-map key/value payloads and configuration-lease payloads also use this
allowance. Label and protocol UTF-8 contents are reserved per admitted
configuration, once across the channel and its queued `Opened` event. Distinct
configurations reserve their own contents even when caller-provided Arc strings
share backing. Local admission checks the channel, configuration and reset costs
before advancing its lifetime or sending DCEP; insufficient storage returns the
unchanged configuration with `WouldBlock`. Remote admission checks before
installing a channel or acknowledging DCEP. Channel removal releases its record,
but shared configuration credit lasts until the final native channel/event
holder leaves. Dequeue, stale drop or abort ends the event's hold; caller-retained
configurations are outside this owner scope.
Event-deque backing also uses this allowance, charged by actual capacity times
the native queued-record size. Channel admission prepays slots for `Opened`,
`Closed` and one reset outcome before lifetime or DCEP mutation. Closing before
open cancels the unused `Opened` promise. Received messages reserve storage without
displacing those notifications; exhaustion resets only their stream, including
for empty messages. Prospective growth uses the same fallible, staged buffer
reservation as ingress: allocation failure or insufficient credit leaves the
old buffer unchanged. Capacity is reused while notifications remain possible,
and released when both queued events and future promises are empty, or on abort.
This replaces SCTP's derived event-count ceiling without changing its per-call
work quantum. The allowance excludes tree/allocator/Arc headers and native
socket/reassembly storage; it is not an RSS bound. Local and remote channel
admission use this funded storage without a separate count ceiling.

`ConnectionLimits::max_transport_output_bytes` independently funds the SCTP
plaintext outgoing stage as well as the protected transport queue. These are
separate allowances using the same configured value, not an aggregate bound.
SCTP packet admission checks retained `Vec` capacity, including spare bytes, plus
actual packet-deque backing through the staged buffer-reservation helper. It
neither copies nor shrinks a packet to fit. Transfer out of this stage ends the
packet charge; live channels reuse the funded deque backing, which is released
when no channels or packets remain, or on abort. This replaces the SCTP
256-packet backlog ceiling; `MAX_WORK_PER_CALL` remains only a work quantum.
The native socket has already committed emitted packets, so inability to retain
one fails the association closed rather than silently losing it. Prepaid cutoff
notifications use their separate runtime allowance and remain deliverable.
Native socket/reassembly and subsequent DTLS/provider staging are outside this
plaintext-stage ledger; caller-retained packets are outside it after transfer.

The same setting separately funds preselection inbound DTLS packet capacity and
physical queue backing. UDP and validated RFC4571 payloads are borrowed through
classification and selected-path handling; pending admission reserves backing
and owned packet capacity before copying. Exhaustion drops only the new packet,
without discarding queued work or failing the transport. The existing 64-packet
preselection window independently bounds synchronous replay work. Selected-tuple
replay transfers the queue into temporary processing and ends its retained
charge; ICE restart, close and failure discard it. Temporary replay and provider
staging remain outside this allowance, which is neither shared with output
stages nor an aggregate transport, provider-handshake or RSS bound.

Closing a channel fences queued delivery immediately. `DataChannelEvent::Closed`
is a cutoff notification, not permission to reuse its stream ID. Reuse requires
`ResetComplete`: both reset directions have completed and the native receive
queue has drained. An explicit open before that barrier returns `ResetPending`
with the unchanged configuration; retry after `ResetComplete`. `ResetFailed` keeps
that stream unavailable with `InvalidState` until association teardown. Late queued events and send-completion accounting remain attached to
their original channel lifetime. Callers must also fence callbacks and messages
already handed to their application before retiring its logical handle.

## Configure media after bootstrap

SDP remains the initial native, WHIP and WHEP interface. Set
`ConnectionConfig::rtp_capabilities` to apply the bounded exact codec-parameter,
feedback, format-scoped repair and extension envelopes during initial answer
construction and retain that same envelope for typed updates. Unsupported offered
formats and their RTX mappings are not accepted. Leave this option unset to retain
the SDP-only bootstrap contract, then declare ongoing support with
`set_rtp_capabilities`, including after an SCTP-only start. Declaring capabilities
does not itself admit typed media; the first configuration or patch below selects
the admitted bindings. Each `RtpCodecCapability` pairs its
parameters with that format's `repair` support; RTX support from another format
does not permit repair on the selected codec. This replaces the connection-wide
`RtpCapabilities::repair` field. This also initializes TWCC for an
SCTP-only bootstrap. An explicitly empty codec envelope permits an empty typed
configuration and rejects every RTP binding. The envelope remains fixed: declare
any media support needed later in the initial envelope, because an installed
empty envelope cannot be expanded. It does not reconstruct ICE, DTLS, SCTP or
feedback history. The typed path currently requires TWCC; bootstrap SDP still
supports RFC 8888.

Use `configure_rtp` for a complete desired `RtpConfiguration`, or `update_rtp`
for independently prepared groups. `inbound` and `outbound` are from RTC's perspective. Each `RtpBinding` supplies its MID, media
kind, selected codec/FMTP/PT/RTX and extension maps, complete primary/repair
SSRCs with optional RIDs, and actual `RtcpParameters` (CNAME and negotiated
`reduced_size` mode). CNAME must be nonempty, at most 255 UTF-8 bytes, and contain
no whitespace or control characters. Missing facts are not filled from defaults.
CNAME and RTCP mode belong to the physical lifetime; changing either for a live
SSRC is a collision, including while paused. Sender Reports carry the configured
source CNAME. Receiver reports and feedback carry the connection's own local
reporter identity, never the remote source CNAME. All outgoing RTCP uses compound
SR/RR and SDES framing, which is also valid when reduced-size RTCP is permitted.
Bootstrap sender and reporter CNAMEs use the value selected for the local SDP
answer; missing remote CNAME facts remain missing.
SSRCs use the full `u32` domain, including zero;
`repair: None` is distinct from `repair: Some(0)`. Opus, VP8, H.264
packetization-mode 1 and VP9 profile 0/2 parameter envelopes are supported. Capabilities are explicit server support,
not a general codec negotiator. No ongoing server SDP is constructed or parsed.

Use `update_rtp(RtpConfigurationPatch)` for addressed edits, including first admission.
Each supplied inbound or outbound binding replaces that direction's MID group;
encodings omitted from that group are permanently retired. Unlisted groups are
unchanged. `remove_encodings` retires selected primary SSRCs in either direction,
preserving other encodings in their group. Removing the last encoding removes
the group. Already removed or unknown primaries are no-ops, so late cleanup for
an old physical lifetime cannot remove a fresh same-MID replacement. Repair
SSRCs are not removal selectors. Duplicate direction/MID entries or a primary
both removed and supplied are invalid. The first patch pauses unmatched bootstrap
candidates without retiring their identities; later patches can adopt their actual
facts independently. No unadopted bootstrap media is admitted. Missing CNAMEs remain
unknown until supplied by preparation. Adopting a MID retires unused bootstrap
identities in that group. `configure_rtp` instead retires all omitted bootstrap
candidates. Both paths preserve actual indices and replay state on adoption.

For preparation outside the runtime owner, obtain `rtp_binding_compiler()` from
the connection or construct `RtpBindingCompiler` from the same validated
`RtpCapabilities`. `compile` returns an opaque immutable binding description;
`compile_patch` and `compile_configuration` prepare addressed or complete sets.
Apply them with `update_compiled_rtp` or `configure_compiled_rtp`. Compilation
validates static codec/FMTP/extension facts and prepares extension ordering, not
live admission. A compiled description can be reused, but cannot reserve an SSRC,
prove installation or bypass current capacity and cross-binding constraints.
Installation rejects a different capability envelope, including different limits.
The direct APIs compile locally and use the same runtime transaction. Callers
remain responsible for connection/binding incarnation and revision fencing.

Both paths stage both directions together before commit. Unknown
identities are not admitted by sender discovery. Invalid formats, inconsistent
mappings, collisions and capacity errors preserve the prior accepted set.
Accepted bindings are owned in indexed records. `rtp_configuration()` explicitly
materializes an owned snapshot, with cost proportional to the live set; no full
snapshot is kept as mutable execution state or reconstructed for addressed edits. `SessionInfo` remains the immutable
bootstrap snapshot, not a second mutable registry. `EncodingDiscovered` carries
the primary payload types, normalized codec names and clock rates from actual
ingress admission. Use those encoding-scoped facts for packet and Sender Report
conversion after bootstrap, rather than looking up new MIDs in `SessionInfo`.
Before the first typed update,
`rtp_configuration()` returns `None` and bootstrap facts apply. After a first patch,
the snapshot contains only accepted typed groups, not paused bootstrap candidates.

An encoding's `active` flag pauses admission without changing its identity.
Use `set_rtp_encoding_active(primary, active)` for local operational changes to
an installed encoding without compiling or rebuilding a binding. Repair SSRCs,
unknown primaries and retired identities are not activity selectors. This updates
only the addressed runtime and admission indexes, preserving unrelated bindings.
Pausing an outbound encoding discards queued originals. Both directions preserve
physical state, primary/RTX numbering, protection and connection-wide feedback
history for resumption.

Omitting an encoding permanently retires it: admission stops and its sender or
receiver disappears from `stats().senders` or `stats().encodings`. Outstanding
buffers stay charged until bounded polling reclaims them. Inbound omission also
fences declared primary/repair SSRCs that have not yet been observed. A replacement
needs fresh primary/repair SSRCs under the current key; a retired identity cannot
be readmitted or retargeted to another MID or direction.

Declaration facts belong to live bindings. Retired bindings retain only issued
SSRC/direction guards and lifetime counters, not codec, MID or extension records.
`RtpConfigurationLimits::max_live_metadata_bytes` bounds the retained capability
envelope, live typed declarations and their MID/encoding index payloads together
(8 MiB by default). Capability charges include codec vector capacity, owned
codec/FMTP/feedback payloads and extension URI entries/string capacities. Format
choices and supported URI sets have no separate count ceiling; actual packet
payload types and extension mappings still obey their wire namespaces. The live
declaration charge includes string/vector capacities (including CNAME), format
key/value payloads, the compiled extension-order index and the separate MID key contents. Each ingress/egress fact-owned CNAME copy is
also charged to that owner's existing runtime allowance. Removing a binding
releases its declaration charge; pausing retains it. The immutable capability
charge remains until the connection is dropped. An over-budget capability
declaration leaves the existing envelope and feedback profiles unchanged. `max_identity_guard_bytes` separately bounds issued SSRC/direction tuple
payloads (1 MiB by default). Retirement never frees those guards under the same
connection/key. Exhaustion rejects new bindings without forgetting history or
changing the accepted set.

These ledgers exclude allocator/tree-node overhead, shared format indexes,
staging copies and other media owners' runtime/index/guard storage. They are not
total memory or RSS bounds. Typed binding and encoding admission has no separate
count ceiling. Declaration and identity ledgers, receive/send runtime budgets,
replay allowances and queued-media budgets govern admission at their respective
owners. Backend staging preflights fact, index, source-state, guard and notification
storage before committing; a failed stage preserves the accepted set and unused
identities. Wire, codec, extension and capability constraints still apply.

`RtpConfigurationLimits::max_active_encodings` has been removed in this pre-v1
API change. Remove that field from struct initializers and configure the byte
allowances above instead. `ConnectionLimits::max_unsignaled_encodings` has also
been removed. Bootstrap discovery uses receive-runtime, event, replay and
queued-media allowances instead of a source-count quota; strict typed source
matching is unchanged. Remove that field from initializers as well. The legacy
`DEFAULT_MAX_UNSIGNALED_ENCODINGS` and `HARD_MAX_UNSIGNALED_ENCODINGS` constants
remain compatibility values, not admission limits.

Inbound replay state has separate `ConnectionLimits::max_rtp_replay_state_bytes`
and `max_rtcp_replay_state_bytes` allowances, each 1 MiB by default. These account
key/window tuple payload storage, not allocator/index overhead or process RSS.
RTP reservations cover currently declared primary and repair SSRCs, including
paused encodings, plus authenticated unreserved bootstrap state. Typed admission
checks the resulting byte allowance before committing; rejection preserves the
accepted configuration. New incoming replay state that exceeds its allowance is
dropped without closing the transport or blocking existing admitted traffic.
Terminal RTP retirement installs a permanent SSRC fence before releasing its
window; SRTCP replay windows remain scoped to the key lifetime. The fixed
512-state crypto ceiling is removed, but this is not complete RTP memory
accounting: runtime/index/guard storage belongs to its separate owner ledgers,
and allocator/RSS exclusions still apply.

The media queue and retransmission allowances keep their 8 MiB and 16 MiB
defaults, but configuration is no longer capped at 64 MiB and 128 MiB.
`ConnectionLimits::HARD_MAX_QUEUED_MEDIA_BYTES` and
`ConnectionLimits::HARD_MAX_RETRANSMISSION_BYTES` remain legacy compatibility
values, not validation ceilings. Increasing the queue allowance also raises the
ingress RTP/RTCP input-length policy described below. Encoded outgoing packet-width
constraints and independent runtime storage budgets are unchanged.

For ingress, `ConnectionLimits::max_queued_media_bytes` bounds retained RTP `Vec`
capacity and also limits individual RTP/RTCP input lengths. Payload and negotiated
extension `Bytes` slices share one wire allocation, charged once if any nonempty
slice retains it. Spare capacity, unused headers and padding stay charged with
that allocation; empty slices alone retain none. RTX charges its normalized
primary packet's actual capacity, not the temporary input allocation. Admission
checks this space before creating a receiver, advancing its replay state or
learning a repair association. Retirement keeps queued backing charged until
polling discards it. Dequeue, including delivery to the caller, or abort ends the
owner's charge; caller-retained packets are outside this queue scope. Event-deque
backing, owned string contents and extension-vector storage are funded by the
receive-runtime allowance below. RTP event admission uses these byte budgets,
not a fixed record ceiling. Stale-event discard work remains bounded per poll;
remaining events request immediate service. Sender Report notifications use the
same event-byte ledger; RTCP parser limits remain. This is not a process-memory
bound.

`ConnectionLimits::max_receive_runtime_bytes` funds stored media-facts registry
tuples and their owned boxed MID, format, feedback, repair, SSRC, RID and extension
payloads, including nested strings. The same 64 MiB default allowance covers
primary receiver/reservation tuple payloads, owned RID contents and the full
bounded NACK backing store. Known bootstrap primaries reserve before acceptance;
typed primaries reserve before configuration commits, even before the
first packet and while paused. Unreserved bootstrap discovery consumes remaining
bytes or drops the input without disturbing admitted receivers. Adopting a live
bootstrap receiver retains its existing receiver reservation while narrowing can
release removed facts payloads. Pause retains facts and receiver reservations;
terminal receiver retirement releases receiver storage, not its media facts.
Configuration omission/removal releases those facts, and abort releases both.
Queued packets remain independently charged until polling. Projection visits only
removed and new media keys; failed staging leaves the live allowance unchanged.

Each media key owns encoding-membership and RID-lookup group headers, even when
empty. Their charge lasts until media removal, not the last receiver's retirement.
Active-set and eligible MID-candidate slots are reserved across pause/resume.
Receiver reservations also fund the per-media set member and a possible owned RID
lookup key, including its second copy of RID contents. Bootstrap reserves the
RTP extension's 255-byte RID maximum; typed receivers use their declared bound.
Late RID learning needs no new allowance. Repaired RID alone selects or checks a
primary during repair processing; it does not learn a primary RID.

Media facts reserve declared-primary index tuples and distinct declared repair
ownership tuples until media removal. Receiver reservations fund primary SSRC
ownership and, when repair payloads are supported, one future repair-link tuple.
Learning an undeclared repair also needs its ownership tuple before association;
that charge joins the primary receiver's lease and is released with it. Repeated
repair packets reuse the charge. Bootstrap adoption transfers learned repair
ownership into declared media storage without charging the same entry twice, while
preserving the receiver's original NACK and RID allowance.

Every issued SSRC also reserves two `u32` terminal-guard payloads before activation:
one for ingress's terminal set and one for the transport's pre-unprotect fence.
This includes declared repairs that have not sent a packet. Discovery and learned
repair identities need this space before changing their indexes. Repeated
associations reuse the reservation; pause, retirement and media removal never
release it. The transport's active-SSRC membership uses its future fence allowance
until retirement removes that membership. Retirement converts a live promise into
both permanent guards without needing free bytes. Only abort releases these guard
reservations. Configuration
preflight charges distinct new identity keys, without scanning all live sources.
An RTX packet cannot reclassify a declared or observed primary as a repair source.

The same allowance funds actual event-deque capacity, including spare slots.
Each receiver reservation promises one future retirement notice in addition to
queued records. Admission allocates and checks a separate candidate buffer before
committing growth; configuration projects only touched receiver leases and
retirements. Retirement converts a promised slot into a queued notice without
allocating, including remote BYE and configuration removal. Sender Reports reserve
their event slots before updating receiver state; CNAME does not create an event.
Empty backing stays charged while receiver promises remain. It is released after
the last queued event and promise are gone, or on abort.

Queued discovery and RTP events also retain charges for their MID/RID/extension
URI string contents and actual extension-vector capacity. Extension values share
packet backing and are charged only by the queued-media allowance. Metadata size
is checked before allocating URI copies; receiver, learned repair identity,
permanent guards and event storage are reserved together before receiver/replay
mutation. Rejected packets do not learn RID or repair associations. A first FID
repair can establish its primary and repair association in that same acceptance.
Retirement does not credit queued metadata: dequeue, terminal filtering or abort
ends its charge, including when the caller retains delivered packets.

Locally generated receive feedback also funds its receipt deque's actual
capacity. A selected feedback profile preallocates the existing 512-receipt
history before connection acceptance or late RTP capability declaration, including
an SCTP-only start. Insufficient budget rejects acceptance or capability declaration
without changing the active profiles. No feedback profile needs no receipt storage;
TWCC packets without a transport sequence are ignored. Observation uses prepaid
storage before media admission, so a packet dropped by media backpressure can still
be reported as physically received. History clear keeps the funded backing for
reuse; abort releases it. The 512-receipt retention policy and 256-status TWCC report
chunks are unchanged.

Incoming RTCP feedback, repair and keyframe queues share this allowance. It funds
all three deques' actual capacities, including spare slots, plus owned TWCC status
arrays and RFC 8888 report/status arrays. Admission stages the minimum storage for
all queues and Sender Report events together, then uses remaining headroom for
amortized growth. Failure drops the batch and its report receipts before Sender
Report mutation; BYE retirement remains effective even when that batch is dropped.
Dequeue credits feedback payloads; the last pop releases that queue's backing.
Abort releases all three buffers. These requests concern outgoing media, so
incoming-source retirement does not discard them. Feedback, repair and keyframe
backlogs have no separate record-count ceiling; they use this byte allowance.
Sender Report notifications likewise use funded event-deque backing, including
future retirement slots, rather than a fixed queued-event ceiling. Each connection
poll processes at most 256 queued keyframe requests; remaining requests stay
charged and request immediate service.
RTCP input/parser limits and other service quanta are unchanged.

This receive-runtime allowance excludes allocation/Arc header and tree overhead,
temporary parsing/staging copies, queued packet backing and replay windows. It
does not replace RTCP parser bounds.
Bootstrap source discovery has no separate count quota. Typed declarations,
replay state, egress and queued media retain their separate storage allowances.
It is not a total-memory or RSS bound.

`ConnectionLimits::max_send_runtime_bytes` reserves sender record/facts/RID
payloads, sender index entries, FIR requester and both repair-map entry payloads,
retirement-container backing, media-queue backing and owned extension-vector
backing, plus retained-original buffers and cache indexes (64 MiB by default).
Bootstrap and typed sender admission check this allowance before acceptance.
Pausing transfers a pre-reserved cleanup record with the canceled work; admitting
fresh queued work or repairs may need another record and can backpressure until
maintenance returns capacity. Terminal removal needs no new reservation. Existing
paused jobs also reclaim a removed sender's cache. The retirement container is
charged by actual allocated capacity, including spare slots. Each live cleanup
promise and outstanding job owns one of those funded slots, without a second
per-record charge. Admission grows this backing before accepting new promises;
pause and terminal retirement therefore need no allocation to enqueue cleanup.
Cleanup returns a promise to an unreserved live sender or releases that obligation.
Spare capacity stays charged while any obligation remains; the backing is dropped
when the last obligation disappears at commit or maintenance. Shutdown preserves
backing needed by surviving live promises.

Sender admission also reserves the entry payloads for its SSRC/MID lookups,
possible RTCP/readiness memberships, queue eligibility and scheduler deficit.
Those later entries need no new reservation and remain funded across pause.
A shared MID key, set header and string are charged once per nonempty group;
removing the last member releases that group charge. Issued primary and configured
repair SSRC guard payloads stay charged after retirement, so repeatedly replacing
senders with fresh identities can exhaust this allowance even at a stable live
sender count. Updates inspect only touched MID groups, not historical identities.

Media queues and owned extension vectors are charged by their allocated capacity,
including unused slots, rather than just their lengths. Queue growth and extension
backing are admitted together with any needed cleanup record; rejection returns
the original write without changing its existing queue or cleanup promise.
Draining a queue releases extension vectors but keeps its backing charge until the
buffer is dropped. Pause transfers that charge to cleanup; shutdown drops deferred
queues but retains the backing of cleared live queues. Original-media admission
has no fixed packet-count quota: it uses the sender runtime allowance, aggregate payload
bytes and the separate 8 MiB pacing-estimate byte limit. Deferred packets remain
charged until cleanup, which still processes at most 256 work items per call.

FIR history uses indexed requester/sequence payloads instead of a fixed requester
count. Unknown requesters cannot displace known duplicate-suppression state on
byte exhaustion; known requesters still advance through sequence wrap. Repair
admission reserves both map entries and any needed retirement record atomically.
Their charges follow each owner independently, including canceled entries still
held by paused cleanup jobs after their global index entry has gone. The fixed
FIR-requester and global repair-request count quotas are removed.

Original commit moves its plaintext packet buffer into optional RTX retention,
without cloning it. Retention funds the buffer's actual capacity, record/expiry
entry payloads and one map header/key per nonempty sender group. Its independent
`max_retransmission_bytes` limit still counts transport wire lengths, not allocator
capacity. Either limit can evict oldest originals. Each retention attempt evicts
at most one maintenance quantum (256 records); if it still cannot fit, the original
remains committed without a cache entry. Removal, expiry and terminal cleanup
release actual retained charges and any matching live repair request; shutdown
releases the cache ledger as a whole.

This sender allowance excludes shared `Bytes`/`Arc` allocations, repair/padding
workspace and transport-owned packet buffers, allocator/tree-node overhead, temporary
growth/staging allocations and other RTC storage. It is partial accounting, not a
total-memory/RSS bound; packet, parser and bounded-service limits remain separate.

Original write admission checks the encoded plaintext length, including generated
MID/RID/TWCC values, extension profiles and four-byte padding. It rejects layouts
above 65,535 bytes before queueing; the encoder reserves the computed length
rather than growing an estimated buffer. Original writes fund actual shared
workspace capacity before acceptance, together with queue and cleanup backing.
Blocked sends return that buffer for reuse; a successful pending send keeps its
charge. Optional retention transfers the pending charge into the cache only when
any replacement workspace also fits, within the same eviction quantum. Otherwise
the original stays committed and its buffer returns to working use. Workspace
capacity is shared across senders and released at shutdown or omission of all
senders. The existing pacing estimate is separate from this layout check, which
is not a network path-MTU guarantee.

`ConnectionLimits::max_sent_history_bytes` defaults to 8 MiB and funds the fixed
sent-packet/TWCC windows, actual controller feedback/synthetic vector capacities,
missing-debt deque capacity (including stale IDs awaiting bounded service) and its
prepaid reorder-deadline keys, source key/value and readiness-index payloads, and each source's allocated
RTP-index/SentPacketId capacity.
Source lookup supports valid late sequence holes without assuming commit order.
Index deletion retains its charge while the table backing remains allocated.
Deadline peeks use the retained packet head and the shared readiness index, not
packet or missing-debt scans. Path changes and reordering-window growth each
visit at most the fixed 32,768-packet window, never the historical source map.
Report clocks carry their path epoch so an unrepresented source resets its
feedback anchors when it next commits on a replacement path.
Each admitted source prepays one readiness key. Permanent omission schedules its
physical sources for bounded idle cleanup at the existing five-second packet-age
boundary, not five seconds after omission. Until then, committed flight and valid
late ACK/loss recovery remain connection-owned. Cleanup releases the source's
header, index backing and readiness key; issued-SSRC guards remain. Paused and
adopted sources retain their sequence and freshness anchors and are not reclaimed.
Prepared source storage without a committed packet can be reclaimed immediately
on terminal omission.
A connection without a feedback profile allocates none of this backing. Bootstrap
or late RTP capability declaration must fund it before enabling the profile;
insufficiency returns `SessionLimitExceeded` at acceptance or `Capacity` on late
declaration, without changing existing profiles or capabilities. Repeated profile
selection and sample clearing retain the backing and do not reset committed history.
Missing-debt growth is funded before recording a new missing entry; geometric
capacity growth is clipped by remaining bytes, not a separate ID count limit.
Expanded controller feedback rows share that allowance, charged by actual vector
capacity rather than capped at the number of incoming status facts. Per-packet
ACK, loss and arrival samples remain distinct and ordered. If required debt or
sample storage cannot fit, the connection fails closed through the existing
`TransportFailure` path before the controller consumes partial input. Queue order,
stale-ID maintenance work, deadlines and loss classification are unchanged.
Clearing debt or samples retains funded backing for reuse. Original, repair and
padding RTP must fund source storage and pass history admission before SRTP
protection; packet IDs, sequences and flight bytes change only on final commit.
Source storage can remain funded for reuse if a later protection attempt fails.
Path changes retain this capacity without reusing source identities. BTreeMap node
and spare-slot overhead, hash-table control and excess buckets, allocator overhead
and temporary staging remain outside this allowance; this is partial accounting,
not an RSS bound. Packet-window,
parser/input-status, ACK-edge interval and synthetic-work limits are unchanged.

Secure RTP/RTCP preparation derives wire and initialized buffer lengths from the
selected native cipher. The final packet buffer, including ICE-TCP prefix and CTR
scratch space, is allocated fallibly before protection. Protection writes into that
caller-owned allocation without growing it or allocating another framing buffer.
Provider-internal storage is outside this packet-storage contract. It rejects
bodies above ICE-TCP's 65,535-byte framing limit
before reserving a queue slot or protecting a packet; an SRTCP rejection does not
consume its index. UDP has no framing limit at this seam, not a path-MTU guarantee.
`ConnectionLimits::max_transport_output_bytes` defaults to 8 MiB and funds
queued packet Vec capacity for all datagram kinds and actual output-queue backing.
The SCTP plaintext stage and preselection inbound DTLS queue independently apply
this same configured value, as described above; it is not an aggregate across
these three stages.
This replaces the transport queue's private 256-packet ceiling.
Secure RTP/RTCP reserves packet and queue storage before protection. Byte
exhaustion returns backpressure without consuming an SRTCP index, discarding
queued output or failing the connection. Aborted growth leaves the live queue
unchanged; an empty queue can release spare backing under pressure.

The ledger stays single-owner: popping a packet transfers its allocation through
commit into caller-owned `Bytes` without copying and ends its queued-output charge.
Failed commit drops the transferred allocation. Caller-retained output and its
clones are outside this connection-owned queue allowance; no shared mutable
counter or new per-packet refcount owner is introduced. Queue spare capacity stays
charged until actually released. Packet capacity includes retained CTR workspace,
not merely truncated wire length. The allowance excludes allocator overhead,
temporary growth/staging overlaps, provider-internal crypto storage and pending
inbound DTLS; it is not an RSS bound. Required ICE/DTLS/SCTP control output
that cannot be queued still fails the transport under its existing failure policy.
The serializer keeps its defensive framing check.

`write_rtp` addresses an unambiguous active outbound MID. Use
`write_rtp_encoding` with its configured primary SSRC when that MID has multiple
encodings. Packet ownership, primary sequence/timestamp translation and source
selection remain with the caller. Selecting another compatible source behind
a binding does not change this configuration.

`retire_encoding(primary)` and authenticated RTCP BYE permanently retire an
observed inbound primary/repair pair under its current key. They fence queued
delivery, remove the receiver runtime from `stats().encodings`, and reject late
RTP before unprotect. Pending queued bytes stay charged until bounded polling
discards them. A different SSRC cannot take over a still-owned RID or receiver;
after retirement, a fresh SSRC can establish a new receiver for that RID.

## Numbering, clocks, and socket handoff

The caller owns primary RTP numbering. Gaps, wrap, and valid late holes are
allowed. **Never reuse an accepted original extended index under the same
sender/key**, including after discarding pending work. A rejected write retains
its packet and does not consume an index. RTX, TWCC, and RTCP numbering remain
RTC-owned; padding uses negotiated RTX, never invented primary indices.

Normal 16-bit RTP sequence wrap does not permit the 48-bit SRTP index to wrap.
SRTCP's 31-bit index is not extended across rollover: old authenticated
ciphertext remains a replay. RTC stops its connection-scoped SRTCP sender
counter before the primitive can repeat an index under the same key.

Routine timed calls take `Instant`. The initial `Instant`/`SystemTime` anchor is
used only for protocol wall-clock conversion. Incoming Sender Report NTP
seconds/fraction and RTP timestamps remain wire facts. Normalization and
synchronization belong to the caller, not an RTC clock service.

Returning `Transmit` is an irrevocable send commit. Service a bounded batch
promptly, or account unsent output as local loss. Never replay an accepted UDP
batch prefix. Pending TCP bytes belong to the original socket incarnation:
finish a partially written frame there or close that socket. Do not redirect a
saved frame or its suffix to a replacement stream. The adapter owns incarnation
tracking and stale-socket filtering; RTC exposes socket tuples, not a flow
admission/retirement API.

## Negotiation and extensions

The supported bootstrap profile is an ICE-lite initial offer/answer with BUNDLE,
RTCP mux, UDP or passive ICE-TCP, negotiated Opus/H.264, and the existing DTLS and
SRTP profiles. Outbound RTP requires one packet-feedback mode, TWCC or RFC 8888.
Typed ongoing media configuration uses the same transport and execution state.
There is no REMB/fixed-rate fallback, ongoing SDP negotiation, ICE restart, or
TURN client.

RTP values own ordinary headers, payloads, and URI-addressed extensions. RTC
regenerates transport identity and TWCC. The caller supplies media extension
values, including DD, VLA, and explicit playout-delay bytes, when negotiated.
RTC remaps their IDs without interpreting frame/dependency or playback meaning.
Unknown extensions are dropped by default and can be negotiated as bounded
opaque values. See [extension ownership](docs/design.md#rtp-extension-ownership).

## Contributor verification

The RTC owner suite includes controlled native libwebrtc peers, public boundary
tests, and private protocol/congestion proofs. Its Linux x86_64 client uses the
same pinned source build and tracked WebRTC patches as the native agent. Bazel
acquires and builds the SDK; follow the [host prerequisites](../../docs/build.md#host-prerequisites)
and [SDK build ownership](../pulsebeam-libwebrtc-sys/README.md#build-ownership-and-caching).
Virtual time and packet delivery drive real protocol implementations without real
sockets or sleeps. Stored SDP is parser evidence, not browser compatibility.
RFC 8888 retains parser, report, accounting, and ECN coverage, not a native-client
interoperability claim.

`simulation/tests/sdpless.rs` starts one native peer and RTC connection with SCTP
only, then applies the actual client module's local answers before declaring typed
media. It checks Opus/VP8/H.264/VP9 payload attribution, duplex directions,
independent tiers and runtime-observed RID/SSRC/RTX identities, pause/reactivation,
invalid-update isolation, retirement/re-add, and audio/data/TWCC continuity. Its
isolated fragmented VP8 frame must recover through negotiated RTX before any
later video input; exact-binding re-add must retain repair numbering. Compatible
fixture source changes prove transport flexibility, not production routing,
frame rewriting, quality selection or replay policy.

```sh
./bazel test //crates/pulsebeam-rtc:test --nocache_test_results
./bazel test //crates/pulsebeam-rtc:format //crates/pulsebeam-rtc:clippy
```

The warm uncached owner suite has a fifteen-second operational host-time target,
not a correctness threshold. Quantitative modeled-network assertions remain
correctness gates. `src/network_tests.rs` and connection unit tests offer direct
RTP to real native receivers for capacity, competition, queue-sojourn,
application-limited, and feedback-outage proofs. Native ingress, fragmented
video, repair, channel, TCP, and same-seed replay tests cover separate wire
obligations. Production source/frame/cache policy remains in the native SFU's
owner proof, not RTC fixtures.

The existing active-media Criterion matrix remains separate from correctness
acceptance:

```sh
CRITERION_HOME="$PWD/target/criterion" ./bazel run -c opt //:benchmark
```

It measures native capture, direct writes/fanout, transport pumping, authenticated
feedback, queue statistics, and a 500 ms virtual drain for 1/8/32 senders and
burst depths of 1/8. Peer construction and teardown are outside timing; the
bottleneck is 500 kbit/s. It is not production-server throughput or isolated
queue-operation cost.

Follow the [workspace development workflow](../../README.md#development) and
[testing contract](../../docs/build.md#testing-contract). Complete composed
acceptance is `./bazel test //:test`; this crate's suite alone does not establish
production integration or additional browser/platform compatibility.
