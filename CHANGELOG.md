## [unreleased]

### 🚀 Features

- Initialize agent abstraction (#59)
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
- Add debounce after mutating pending state
- Create signaling cid on connect
- Missing start time in agent
- Broken build on mismatch version
- Cargo release config

### 💼 Other

- Tokio alt timer on 1.49

## [pulsebeam-v0.3.1] - 2025-12-29

### 🐛 Bug Fixes

- Allow unpausing with same receiver

### ⚙️ Miscellaneous Tasks

- Bump to 0.3.1

## [pulsebeam-v0.3.0] - 2025-12-24

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

### 💼 Other

- Use p80 instead of ewma
- *(deps)* Bump the github-actions group across 1 directory with 4 updates (#31)

### ⚙️ Miscellaneous Tasks

- Update str0m to 0.14
- More consistent pprof endpoints
- Bump to 0.3.0

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
- Off-by-one nack bug

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

### 🐛 Bug Fixes

- Demo audio flag

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

- Add early IO batch
- Increase concurrency by manual yielding
- Reuse memory allocation on recv
- Improve fanout concurrency
- Instrument actor metrics
- Shared spmc ring buffer (#27)

### 🐛 Bug Fixes

- Independent task monitor
- Read write mix up
- Missing notification
- Benchmark with yielding

### ◀️ Revert

- Multi-thread as default runtime

## [pulsebeam-v0.2.2] - 2025-10-03

### 🚀 Features

- Switch to low-latency rtp mode (#26)

### 🐛 Bug Fixes

- Worker amount is not affected by containerd

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.2

### ◀️ Revert

- Readme docker version

## [pulsebeam-v0.2.1] - 2025-10-01

### 🐛 Bug Fixes

- Readme server version

### ⚙️ Miscellaneous Tasks

- Bump to 0.2.1

## [pulsebeam-v0.1.13] - 2025-09-26

### ⚙️ Miscellaneous Tasks

- Bump to 0.1.13

## [pulsebeam-v0.1.12] - 2025-09-25

### 🚀 Features

- Implement audio top-n
- Tweak actor trait to be fakeable
- Use track fake actor in audio top-n
- Simplify audio scoring

### 🐛 Bug Fixes

- Missing ffmpeg deps
- Add libavfilter
- Add libdevice-dev
- Incorrect libavdevice-dev
- Unused warnings
- No guarantee slot per sender
- Skip agent rt for now
- Actor_loop skipping
- Incorrect active speaker logic
- Login order for docker
- Separated docker build platforms
- Use lowercase owner
- Hardcode repo owner to lowercase
- Apple release build
- Use arm ubuntu runners for arm
- Docker build on tag & use m1
- Docker ci and prune cargo-dist members
- Use wincrypto for windows build
- Use msys2 for windows
- Missing nasm dep in ci

### ⚙️ Miscellaneous Tasks

- Bump to 0.1.12
- Make demo step-by-step
- Use ubuntu for windows

## [pulsebeam-v0.1.11] - 2025-09-19

### 🐛 Bug Fixes

- Https in log

### ⚙️ Miscellaneous Tasks

- Bump to 0.1.11

## [pulsebeam-v0.1.9] - 2025-09-19

### 🐛 Bug Fixes

- Invalid github workflow
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
- Use official rust alpine build

### 🐛 Bug Fixes

- Incorrect entity prefix
- Test using observable state
- Add missing biased select
- Add missing track state management
- Sink buffer size and use flume
- Skip agent for now
- Warnings and simplify audio selector
- Cargo license
- Add release subcommand
- Incorrect dep
- Add crate to allow release-plz to release
- Missing sudo
- Docker workflow
- Incorrect docker action dep
- Image name must be all lowercase
- Remove timeout
- Dockerfile
- Vite compile error
- Volume path
- Broken dockerfile
- Smart-release missing username
- Incorrect syntax
- Incorrect step
- Add privilege to release bot

### 💼 Other

- But whip whep works

### ⚙️ Miscellaneous Tasks

- Release v0.1.0
- Release v0.1.2
- Remove cd comment
- Move build.rs
- Bump to 0.1.3

## [0.0.3] - 2025-05-14

### 🐛 Bug Fixes

- Start button listener
- Incorrect pt on cross clients
- High cpu on incorrect timing

### 💼 Other

- Use SDN architecture
- Propagate media and subscriptions

## [0.0.2] - 2025-04-20

### 🚀 Features

- Migrate to grpc and grpc-web
- Prefix grpc transport with /grpc
- Add ping endpoint
- Make RecvStream public
- Move cors to grpc routes only
- Use bounded queue
- Use rendezvous queue and discovery ch
- Add safeguard for loopback
- Separate hot-path from connection index
- Expose connection count
- Decouple conns from index managers
- Allow custom indexer
- Add event hook
- Simplify with smaller event_handler trait
- Update protocol with latest analytics

### 🐛 Bug Fixes

- Make sure the query result is stable in tests
- Undrained index worker
- Potential panic on eviction
- Remove unused tokio in test
- Broken tests

### 💼 Other

- Move validation to a separate struct

## [0.0.1] - 2025-03-03

### 🚀 Features

- Initial commit
- Seaparate server impl from main
- Init tests
- Add recv feat timeout
- Bound server with capacity and add latency tolerance
- Configurable server
- Use rust 2021 edition
- Add protoc setup
- Add cors, ping, trace
- Add public stun
- Use http to vendor proto
- Init docker build
- Add server log env
- Use future moka and add use after ttl test
- Best-effort message deduplication
- Use unbuffered channel
- Instrument receiving loop
- Add session batch max
- Remove unnecessary id hash
- Add developer preview warning
- Rename project

### 🐛 Bug Fixes

- Missing stun scheme
- Missing BIN arg
- Make binary available for builder
- Incorrect ENTRYPOINT
- Docker build
- Remove incorrect warnings
- Recv_after_ttl test
- Unstable test due to ordering
- Rename to foss
