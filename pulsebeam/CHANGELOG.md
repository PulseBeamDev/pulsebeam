## [pulsebeam-v0.3.5] - 2026-03-18

### 🐛 Bug Fixes

- Audio loopback
## [pulsebeam-v0.3.4] - 2026-03-18

### 🚀 Features

- Use connection_id as etag
- Use memory pool

### 🐛 Bug Fixes

- Inverted power of 2 spmc
- Missing hex for test
- Unit and sim tests
- Incorrect hysteresis cost
- Remove unsupported mpsc tests
- Axum task leaks
- Missing console_subscriber on default
- Potential spin loop on large switching
- Missing budget sub
- Sim tests
- Some warnings
- Missing waker on spmc
- Loom spmc tests
- Missing presorted slot views
- Undefined quality bug
- Stuck stream state transition

### 💼 Other

- Add participanttaskresult type
- Define runnable first
## [pulsebeam-v0.3.3] - 2026-01-28

### 🚀 Features

- Sliding window bitrate and start paused
- Isolate sim flag into core (#61)
- Use http client trait with adapters
- Add signaling to agent and sim

### 🐛 Bug Fixes

- Test to allow 10% bitrate deviation
- Missing turmoil flag in Cargo
- Core license
- Backward udp mode
- Track id validation test
- Missing mid padding
- Broken build on mismatch version
- Cargo release config

### 💼 Other

- Tokio alt timer on 1.49

### ⚙️ Miscellaneous Tasks

- Bump to 0.3.3
- Release
## [pulsebeam-v0.3.1] - 2025-12-29

### 🚀 Features

- Reduce atomic usage on spmc receiver
- More aggresive quality upgrade (#46)
- Add downgrade phase to downstream allocator (#47)
- Tcp ice candidate
- Split net reader and writer (#49)

### 🐛 Bug Fixes

- Warnings
- Broken fir backoff
- Bitrate test
- Allow unpausing with same receiver

### 💼 Other

- Use p80 instead of ewma

### ⚙️ Miscellaneous Tasks

- Update str0m to 0.14
- More consistent pprof endpoints
- Bump to 0.3.0
- Bump to 0.3.1
## [pulsebeam-v0.2.25] - 2025-12-13

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.25
## [pulsebeam-v0.2.24] - 2025-12-13

### 🚀 Features

- Add memory profile and jemalloc

### 🐛 Bug Fixes

- Metrics-rs unbounded memory and cpu usage
- Docker build with jemalloc

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.24
## [pulsebeam-v0.2.23] - 2025-12-05

### 🐛 Bug Fixes

- Handle track cleanups

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.23
## [pulsebeam-v0.2.22] - 2025-12-02

### 🚀 Features

- Io batch read forwarding pipeline (#42)

### 🐛 Bug Fixes

- Spmc race condition with fast consumer

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.22
## [pulsebeam-v0.2.21] - 2025-11-24

### 🚀 Features

- Rework keyframe buffer implementation

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.21
## [pulsebeam-v0.2.20] - 2025-11-21

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.20
## [pulsebeam-v0.2.19] - 2025-11-21

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.19
## [pulsebeam-v0.2.18] - 2025-11-21

### 🐛 Bug Fixes

- Backward deadline calculation

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.18
## [pulsebeam-v0.2.17] - 2025-11-20

### 🚀 Features

- Slot based stream driver (#38)
- Use simpler rwlock in spmc

### 🐛 Bug Fixes

- Incorrect video priority

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.17
## [pulsebeam-v0.2.16] - 2025-11-15

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.16
## [pulsebeam-v0.2.15] - 2025-11-15

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.15
## [pulsebeam-v0.2.14] - 2025-11-15

### 🚀 Features

- Forward using server-synced playout time (#36)
- Synchronized playback

### ⚙️ Miscellaneous Tasks

- Reduce track_id scope
- Bump to 0.2.13
- Bump to 0.2.14
## [pulsebeam-v0.2.12] - 2025-11-11

### 🚀 Features

- Adaptive inactivity

### 🐛 Bug Fixes

- Spin loop on invalid argument

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.12
## [pulsebeam-v0.2.11] - 2025-11-11

### 🐛 Bug Fixes

- Incorrect pt for different profiles

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.11
## [pulsebeam-v0.2.10] - 2025-11-10

### 🚀 Features

- Delta-of-delta based upstream quality score (#34)
- Add downgrade hysteresis

### 🐛 Bug Fixes

- Log lagging receiver on transition

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.10
## [pulsebeam-v0.2.9] - 2025-11-05

### 🚀 Features

- Add reordering on receiving end (#30)
- Rework sequencer (#32)
- Formal simulcast quality type

### 🐛 Bug Fixes

- Stuck allocator and panic on quality change
- Incorrect keyframe kind from client
- Panic on same active and fallback
- Incorrect frequency for audio
- Fail to switch layer on keyframe
- Off by 1 GRO tail corruption

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.9
## [pulsebeam-v0.2.8] - 2025-10-29

### 💼 Other

- Create track in core
- Remove unnecessary indirection

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.8
## [pulsebeam-v0.2.7] - 2025-10-28

### 🚀 Features

- Rework simulcast layer switching (#29)

### 💼 Other

- Move rtp rewriter to track reader

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.7
## [pulsebeam-v0.2.6] - 2025-10-25

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.6
## [pulsebeam-v0.2.5] - 2025-10-24

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.5
## [pulsebeam-v0.2.4] - 2025-10-24

### 🐛 Bug Fixes

- Warnings in track
- Keyframe request throttler
- Discontinous rtp sequence on layer switching
- Slow graceful bye
- Rtp_rewriter tests
- Jitter buffer tests

### 💼 Other

- Remove switch point

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.4
## [pulsebeam-v0.2.3] - 2025-10-21

### 🚀 Features

- Switch to low-latency rtp mode (#26)
- Add early IO batch
- Increase concurrency by manual yielding
- Reuse memory allocation on recv
- Improve fanout concurrency
- Instrument actor metrics
- Shared spmc ring buffer (#27)

### 🐛 Bug Fixes

- Worker amount is not affected by containerd
- Independent task monitor
- Read write mix up
- Missing notification
- Benchmark with yielding

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.2

### ◀️ Revert

- Multi-thread as default runtime
## [pulsebeam-v0.2.1] - 2025-10-01

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.1
## [pulsebeam-v0.1.13] - 2025-09-26

### 🚀 Features

- Implement audio top-n
- Tweak actor trait to be fakeable
- Use track fake actor in audio top-n
- Simplify audio scoring

### 🐛 Bug Fixes

- Unused warnings
- No guarantee slot per sender
- Actor_loop skipping
- Incorrect active speaker logic
- Use wincrypto for windows build

### ⚙️ Miscellaneous Tasks

- Bump to 0.1.12
- Bump to 0.1.13
## [pulsebeam-v0.1.11] - 2025-09-19

### 🐛 Bug Fixes

- Https in log

### ⚙️ Miscellaneous Tasks

- Bump to 0.1.11
## [pulsebeam-v0.1.9] - 2025-09-19

### 🐛 Bug Fixes

- Mutable references and some tests
- Warnings on unused
- Fake actor tests with yield

### 💼 Other

- Simplify participant

### ⚙️ Miscellaneous Tasks

- Bump to 0.1.9
## [pulsebeam-v0.1.8] - 2025-09-18

### 🐛 Bug Fixes

- Batching ownership and socket config
- Hot loop in sending and stuck on reading
- Missing outgoing packets
## [0.1.0] - 2025-09-11

### 🚀 Features

- Separate io and cpu runtimes
- Bump to 0.1.2

### 🐛 Bug Fixes

- Test using observable state
- Add missing biased select
- Add missing track state management
- Sink buffer size and use flume
- Warnings and simplify audio selector
- Add crate to allow release-plz to release

### 💼 Other

- But whip whep works

### ⚙️ Miscellaneous Tasks

- Release v0.1.0
- Release v0.1.2
- Move build.rs
- Bump to 0.1.3
