# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2](https://github.com/PulseBeamDev/pulsebeam/releases/tag/pulsebeam-v0.1.2) - 2025-09-10

### Added

- bump to 0.1.2
- separate io and cpu runtimes

### Fixed

- add crate to allow release-plz to release
- warnings and simplify audio selector
- sink buffer size and use flume
- add missing track state management
- add missing biased select
- test using observable state

### Other

- add release-plz config
- use cd config and set publish
- bump pulsebeam version to 0.1.1
- Merge pull request #1 from PulseBeamDev/release-plz-2025-09-10T17-40-26Z
- disable publish
- Add GNU AGPL v3 License
- reuse workspace deps
- add default log level and endpoint log
- add an initial simulcast support
- add profiling util and fix duplicate stream
- init node lib
- add comment about h264
- improve auto subscription logic
- init select-n audio streams
- handle track unsubscription
- but whip whep works
- add todos to source
- disable ice lite again for OBS
- increase buffer size and add debug logs
- add simple whip example
- add actor id to tracing span
- move participant constructor
- unwrap terminate commands
- use actor_loop macro to implement
- init actor_loop macro
- handle track unpublished and share broadcast
- use hashmap instead of vec for track states
- add tracks collection
- rename actor id to meta
- remove track handle wrapper
- add actor_id to handle
- init spawn api instead
- decouple state from actor for observability in testing
- add publish track test
- init room test with simulation
- init test stub
- more flexible actor trait
- stub out main for now
- box rtc
- use actorfactory prepare and relax trait constraints
- init actor factory
- init interceptor
- use runner concept
- compilable
- add spawn config
- refactor controller and room
- remove crate rng and refactor participant
- refactor track, room, participant more
- rng -> rand runtime
- introduce system_ctx and remove other hooks
- add prelude and convert more actors
- replace some participant stuff
- refactor more dependencies
- add ActorContext to runtime
- refactor source and sink
- init runtime refactor
- init net runtime
- add Send to packetSocket
- use stable and add instrument
- add more from original actor
- restructure and init minimal actor lib

## [0.1.0](https://github.com/PulseBeamDev/pulsebeam/releases/tag/pulsebeam-v0.1.0) - 2025-09-10

### Other

- use release please token
