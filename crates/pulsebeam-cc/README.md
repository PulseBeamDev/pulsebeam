# `pulsebeam-cc`

`pulsebeam-cc` is a caller-driven SCReAMv2 controller and one transport-byte
pacer. It does not own packet history, queues, a runtime, or a clock. The crate
is an internal workspace component. RTC uses it through a thin time/feedback
adapter; other datagram hosts own their own authenticated history and queues.

## Driving the controller

Start a path with an empty observation, then check complete packets before
attempting a send:

```rust
use std::time::Duration;
use pulsebeam_cc::{Config, Controller, EcnValidation, Observation, Readiness};

fn start() -> Result<(), Box<dyn std::error::Error>> {
    let mut controller = Controller::new(Config::default())?;
    let envelope = controller.update(Duration::ZERO, Observation {
        path_epoch: Some(1), path_available: true, feedback: &[],
        feedback_hold: Duration::ZERO, fresh_network_feedback: false,
        bytes_in_flight: 0, paced_queue_bytes: 0, desired_media_rate: 1_000_000,
        ecn: EcnValidation::default(),
    })?;
    assert!(envelope.target_media_payload_rate > 0);
    assert_eq!(controller.ready(Duration::ZERO, 0, 1_200), Readiness::Ready);
    Ok(())
}
```

Use one `Controller` per independently controlled path. Supply monotonic
`Duration`s relative to a host-selected origin, at most `MAX_TIME` (`u64::MAX`
microseconds). An observation carries a path epoch, availability, aggregate
media-payload demand, current queue and flight sizes, and normalized feedback.
Authenticate feedback identity, path membership and replay at the host before
calling `update`. The controller validates the entire batch before changing
state and rejects batches larger than `MAX_FEEDBACK_SAMPLES` (8,448).
Receiver-relative arrival timestamps are signed and need not share the host
clock. Reordered packets and nonreceived ACK-edge gaps are allowed. An available
path needs an epoch; a new epoch cannot be introduced in a batch with feedback.

Check `ready` with the **complete transport packet size**, then attempt the
transport send. Readiness is non-reserving: serialize the check, transport
acceptance and `commit_send`, and commit every accepted packet exactly once.
The infallible commit clamps backwards or out-of-range times to the last valid
host time; it still charges the accepted size. `prepare_padding` proposes a
quantum, not a reservation, so check each complete padding packet in the same
way. Call `padding_unavailable` if the host cannot generate the prepared
padding. `next_deadline` combines actionable pacing, padding and maintenance;
call `process_idle` to advance maintenance without receiver-driven growth.
Supply complete-packet `(flight, bytes)` candidates separately for data and
padding deadlines. Positive window slack alone does not make a packet actionable.
`padding_request_bytes` permits unreserved host-side padding layout estimation;
actual protected lengths must still pass `ready` before acceptance.

`update_queue` adjusts effective pacing for queue drain without changing the
nominal rate or erasing debt. As in the pinned pacer, rate changes reprice
outstanding debt from its last processing cursor, so an increased rate can
make an existing deadline due sooner. Same-epoch outages preserve debt; a new
epoch resets path evidence and pacer debt while retaining the native target. Counters
in `diagnostics` saturate at their numeric bounds. `Config::default()` has no
configured maximum; `Some(0)` is invalid.

`Envelope` retains the original RTC `SafeRtpEnvelope` field names for mapping:
media-payload rates are in bits/s, flight and queue sizes are transport bytes,
and pacing and padding rates are transport bits/s. `pacing_transport_rate` is
the effective queue-adjusted rate; `nominal_pacing_transport_rate` is the
controller rate before that adjustment. The target is not an independent
physical-capacity estimate. The host owns media allocation and authentication.

## Provenance and verification

The Rust sources originate from `pulsebeam-rtc` at repository commit
`14fd653774f61c7bb6de261d306b362fc0be39b4`. They adapt libwebrtc revision
`326fd06bb4d93900ed6947205b94cefd2413afca`. Upstream `LICENSE` and `PATENTS`
are retained in `src/congestion/screamv2/`. `scripts/verify-scream-profile.sh`
checks pinned upstream artifacts and retained notices, not behavioral
conformance. The copied native scenario is a rate-throttled adapter proof, not
a faithful network simulation of this pacer. The separate packet-level virtual-link
fixture drives real readiness, accepted-size commit, bounded host history, receipt
normalization and independent directions. The DTLS owner exercises real ciphertext
preparation, rejected sink submission, retry, acceptance and authenticated receipt.
These virtual links do not establish physical UDP timing or sustainable throughput.

From the repository root, run:

```sh
./bazel test //crates/pulsebeam-cc:test //crates/pulsebeam-cc:format //crates/pulsebeam-cc:clippy
```

Use `scripts/verify-scream-profile.sh [pinned-libwebrtc-source-root]` for source
identity. Missing sources or failed downloads are missing provenance evidence,
not passing behavioral conformance.
