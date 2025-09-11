# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.3](https://github.com/PulseBeamDev/pulsebeam/releases/tag/pulsebeam-v0.1.3) - 2025-09-10

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

- bump to 0.1.3
- move build.rs
- release v0.1.2
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

## v0.1.3 (2025-09-11)

<csr-id-cf1e372142450caab8dadf09820bffac258b2ab4/>
<csr-id-c8b148f561d4de1cd08250e699b4dc760da57397/>
<csr-id-e59f8a72687c9f842ae777bbcaa0f1eedddffefe/>

### Chore

 - <csr-id-cf1e372142450caab8dadf09820bffac258b2ab4/> bump to 0.1.3
 - <csr-id-c8b148f561d4de1cd08250e699b4dc760da57397/> move build.rs
 - <csr-id-e59f8a72687c9f842ae777bbcaa0f1eedddffefe/> release v0.1.2

### Commit Statistics

<csr-read-only-do-not-edit/>

 - 7 commits contributed to the release.
 - 3 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 0 issues like '(#ID)' were seen in commit messages

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **Uncategorized**
    - Release pulsebeam-runtime v0.1.0, pulsebeam v0.1.3 ([`9c73d5b`](https://github.com/PulseBeamDev/pulsebeam/commit/9c73d5bbe6b2a376895ad4f080ded4b1e822e163))
    - Merge pull request #8 from PulseBeamDev/dependabot/cargo/hyper-1.7.0 ([`4e87249`](https://github.com/PulseBeamDev/pulsebeam/commit/4e87249e0f0d6be08f390cdd2dfc126b21ac03bd))
    - Bump hyper from 1.6.0 to 1.7.0 ([`caf98fa`](https://github.com/PulseBeamDev/pulsebeam/commit/caf98fa9a6416bc8a1742bbaee0b2be9dfddc68c))
    - Bump to 0.1.3 ([`cf1e372`](https://github.com/PulseBeamDev/pulsebeam/commit/cf1e372142450caab8dadf09820bffac258b2ab4))
    - Move build.rs ([`c8b148f`](https://github.com/PulseBeamDev/pulsebeam/commit/c8b148f561d4de1cd08250e699b4dc760da57397))
    - Merge pull request #2 from PulseBeamDev/release-plz-2025-09-10T18-55-38Z ([`c60e20c`](https://github.com/PulseBeamDev/pulsebeam/commit/c60e20c82ce03fb824b07e03adb85968abd8a6b5))
    - Release v0.1.2 ([`e59f8a7`](https://github.com/PulseBeamDev/pulsebeam/commit/e59f8a72687c9f842ae777bbcaa0f1eedddffefe))
</details>

## v0.1.2 (2025-09-10)

<csr-id-f5b95d1f0192d19731a1ec8846ff70059f75682b/>
<csr-id-addf3634bce5e3e048a42ccc2f2441f368862a47/>

### Chore

 - <csr-id-f5b95d1f0192d19731a1ec8846ff70059f75682b/> release v0.1.0

### New Features

 - <csr-id-a5e2e152d5cd7478af949de89d436a731e807262/> bump to 0.1.2
 - <csr-id-cad7b8f249b54d63e203bc7f2e35fe24098d0a7c/> separate io and cpu runtimes

### Bug Fixes

 - <csr-id-b0e7fcf7ccfda2c13a3a740a1e8e053279ba4454/> add crate to allow release-plz to release
 - <csr-id-37e49a5feff56268647c8b6a2ce23aaac4f550cc/> warnings and simplify audio selector
 - <csr-id-bd715935ca57b76a93a0c8dc791401f3f5c37f91/> sink buffer size and use flume
 - <csr-id-7015178daed52e7fa33c4a36afe9d49140310a70/> add missing track state management
 - <csr-id-8c0bc910115a7b28d1dd8c311e85446eb5adc1cf/> add missing biased select
 - <csr-id-cba67ea2c5f0b59d82e9817567688a9dc3157a6c/> test using observable state

### Other

 - <csr-id-addf3634bce5e3e048a42ccc2f2441f368862a47/> but whip whep works

### Commit Statistics

<csr-read-only-do-not-edit/>

 - 70 commits contributed to the release over the course of 15 calendar days.
 - 10 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 0 issues like '(#ID)' were seen in commit messages

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **Uncategorized**
    - Add crate to allow release-plz to release ([`b0e7fcf`](https://github.com/PulseBeamDev/pulsebeam/commit/b0e7fcf7ccfda2c13a3a740a1e8e053279ba4454))
    - Add release-plz config ([`c8f2a5d`](https://github.com/PulseBeamDev/pulsebeam/commit/c8f2a5db9962d4730a2a5f0bc9a231abe03061c7))
    - Use cd config and set publish ([`6d2c07e`](https://github.com/PulseBeamDev/pulsebeam/commit/6d2c07e725468ac9507b098c9c12d201b745a758))
    - Bump to 0.1.2 ([`a5e2e15`](https://github.com/PulseBeamDev/pulsebeam/commit/a5e2e152d5cd7478af949de89d436a731e807262))
    - Bump pulsebeam version to 0.1.1 ([`71c8360`](https://github.com/PulseBeamDev/pulsebeam/commit/71c8360db78f9852c0a2323e9df95e9bc504ed73))
    - Merge pull request #1 from PulseBeamDev/release-plz-2025-09-10T17-40-26Z ([`896b23c`](https://github.com/PulseBeamDev/pulsebeam/commit/896b23c243113dd1a2a521dbc273597feb165a29))
    - Disable publish ([`2193397`](https://github.com/PulseBeamDev/pulsebeam/commit/2193397f2002bedcd7acd61eb46ce0755bf951ce))
    - Release v0.1.0 ([`f5b95d1`](https://github.com/PulseBeamDev/pulsebeam/commit/f5b95d1f0192d19731a1ec8846ff70059f75682b))
    - Warnings and simplify audio selector ([`37e49a5`](https://github.com/PulseBeamDev/pulsebeam/commit/37e49a5feff56268647c8b6a2ce23aaac4f550cc))
    - Add GNU AGPL v3 License ([`1685e94`](https://github.com/PulseBeamDev/pulsebeam/commit/1685e941e83f05e908bccdc9eb09bcdcbffa4e98))
    - Reuse workspace deps ([`50e33dd`](https://github.com/PulseBeamDev/pulsebeam/commit/50e33dd7c61d9d688277d421d41dcf2bebe872cc))
    - Add default log level and endpoint log ([`278d514`](https://github.com/PulseBeamDev/pulsebeam/commit/278d514c914d8202f066396063a0f722a4f08e1b))
    - Add an initial simulcast support ([`b810716`](https://github.com/PulseBeamDev/pulsebeam/commit/b81071616a67df6957d67c9668d2e68318909d51))
    - Separate io and cpu runtimes ([`cad7b8f`](https://github.com/PulseBeamDev/pulsebeam/commit/cad7b8f249b54d63e203bc7f2e35fe24098d0a7c))
    - Sink buffer size and use flume ([`bd71593`](https://github.com/PulseBeamDev/pulsebeam/commit/bd715935ca57b76a93a0c8dc791401f3f5c37f91))
    - Add profiling util and fix duplicate stream ([`b078914`](https://github.com/PulseBeamDev/pulsebeam/commit/b078914681ffb86ca0b21b819beb7f2ae1e50b78))
    - Init node lib ([`fab40be`](https://github.com/PulseBeamDev/pulsebeam/commit/fab40be8cbaef3dbffaf6e4b10bc1a0b90444cc8))
    - Add comment about h264 ([`f32d1d7`](https://github.com/PulseBeamDev/pulsebeam/commit/f32d1d703c2365d450f4caf3afc1c25fa35e8d59))
    - Improve auto subscription logic ([`a7a5ea6`](https://github.com/PulseBeamDev/pulsebeam/commit/a7a5ea6e654c7a4eaa7cd384bd8c4ed33e461c44))
    - Init select-n audio streams ([`66a078a`](https://github.com/PulseBeamDev/pulsebeam/commit/66a078ad4b0c542e7764b4662e0c54fe6abfdb4c))
    - Handle track unsubscription ([`6e155b1`](https://github.com/PulseBeamDev/pulsebeam/commit/6e155b1a22786b1ffc11a16ba98317f84c6c286a))
    - But whip whep works ([`addf363`](https://github.com/PulseBeamDev/pulsebeam/commit/addf3634bce5e3e048a42ccc2f2441f368862a47))
    - Add todos to source ([`1f1b5c0`](https://github.com/PulseBeamDev/pulsebeam/commit/1f1b5c0df29144858eb5b1d9a95b83b36172f5d9))
    - Disable ice lite again for OBS ([`4504975`](https://github.com/PulseBeamDev/pulsebeam/commit/4504975f66f1e0364de1414ea8e4d2ce2357de7c))
    - Increase buffer size and add debug logs ([`5fa270b`](https://github.com/PulseBeamDev/pulsebeam/commit/5fa270b3ca53d9d29f462ad817d577af1dd69dc9))
    - Add simple whip example ([`9976387`](https://github.com/PulseBeamDev/pulsebeam/commit/99763874cca79a8f75d51cc153f0f3d0fd43a05a))
    - Add actor id to tracing span ([`60e96b4`](https://github.com/PulseBeamDev/pulsebeam/commit/60e96b4c8325a464050d13a6a0fcaaec16190146))
    - Move participant constructor ([`902a199`](https://github.com/PulseBeamDev/pulsebeam/commit/902a1996c67bbf6033fc3540252cdd904d3e9652))
    - Unwrap terminate commands ([`0833277`](https://github.com/PulseBeamDev/pulsebeam/commit/0833277109abb105e6467d49087fbd3b3cb61637))
    - Use actor_loop macro to implement ([`fe0cc14`](https://github.com/PulseBeamDev/pulsebeam/commit/fe0cc14fe0c76b057bd125e0f7b3f8c84817bd7f))
    - Init actor_loop macro ([`b011f76`](https://github.com/PulseBeamDev/pulsebeam/commit/b011f76ea244f9a09362c28bd90c51cabf362038))
    - Add missing track state management ([`7015178`](https://github.com/PulseBeamDev/pulsebeam/commit/7015178daed52e7fa33c4a36afe9d49140310a70))
    - Add missing biased select ([`8c0bc91`](https://github.com/PulseBeamDev/pulsebeam/commit/8c0bc910115a7b28d1dd8c311e85446eb5adc1cf))
    - Handle track unpublished and share broadcast ([`2a41eff`](https://github.com/PulseBeamDev/pulsebeam/commit/2a41efff719e34fc89acf950b27ed43d993decd9))
    - Use hashmap instead of vec for track states ([`d5d2ef7`](https://github.com/PulseBeamDev/pulsebeam/commit/d5d2ef78a808e2fcb46afd01586a5a4e524f4cb3))
    - Add tracks collection ([`b6be087`](https://github.com/PulseBeamDev/pulsebeam/commit/b6be0870e15851b1df5e350592c3b48ab07b20bc))
    - Rename actor id to meta ([`bc14003`](https://github.com/PulseBeamDev/pulsebeam/commit/bc14003e6f1740888eb9c89f606eb061d745404b))
    - Remove track handle wrapper ([`2b97208`](https://github.com/PulseBeamDev/pulsebeam/commit/2b9720849af75c0359ce90aca6dad4a7ad8b1a43))
    - Add actor_id to handle ([`6da83fe`](https://github.com/PulseBeamDev/pulsebeam/commit/6da83febe06e9d84e9dcb96418817b65f537f008))
    - Test using observable state ([`cba67ea`](https://github.com/PulseBeamDev/pulsebeam/commit/cba67ea2c5f0b59d82e9817567688a9dc3157a6c))
    - Init spawn api instead ([`fb32908`](https://github.com/PulseBeamDev/pulsebeam/commit/fb32908f9569fe9fa58064947a67d06c40a79a4b))
    - Decouple state from actor for observability in testing ([`6076f5e`](https://github.com/PulseBeamDev/pulsebeam/commit/6076f5eef6c22f97eb5d4a267ab3399ec0eaf73a))
    - Add publish track test ([`fbde06b`](https://github.com/PulseBeamDev/pulsebeam/commit/fbde06baf7cfbc95f838003316022212dd97fc03))
    - Init room test with simulation ([`ddf2d7a`](https://github.com/PulseBeamDev/pulsebeam/commit/ddf2d7a1ad4d94a940466e901faa5d6c593f77ab))
    - Init test stub ([`280b426`](https://github.com/PulseBeamDev/pulsebeam/commit/280b4266cb69b63378faffd7b56334097043f2c1))
    - More flexible actor trait ([`227873e`](https://github.com/PulseBeamDev/pulsebeam/commit/227873e7748e00067c468563cd032c774b4c438d))
    - Stub out main for now ([`21990a1`](https://github.com/PulseBeamDev/pulsebeam/commit/21990a12c8ce63bb0548935734e08ba23ae39bb1))
    - Box rtc ([`9e8aa58`](https://github.com/PulseBeamDev/pulsebeam/commit/9e8aa5803bdd318ceac81cefb7973d3c4368c1a9))
    - Use actorfactory prepare and relax trait constraints ([`9318088`](https://github.com/PulseBeamDev/pulsebeam/commit/9318088754dc9b31db0f2ffdc545c3f906ba8676))
    - Init actor factory ([`fc252be`](https://github.com/PulseBeamDev/pulsebeam/commit/fc252be397d6d94b3d84609187beb5584fb391dd))
    - Init interceptor ([`06b7f97`](https://github.com/PulseBeamDev/pulsebeam/commit/06b7f97473997a5a6dc882155553624877b46ee0))
    - Use runner concept ([`3a0c0f5`](https://github.com/PulseBeamDev/pulsebeam/commit/3a0c0f5bb79f5cd6d571c6cf73fc7de55491747f))
    - Compilable ([`c96dcaf`](https://github.com/PulseBeamDev/pulsebeam/commit/c96dcafe442cae75c3f816ce34af4f118a920835))
    - Add spawn config ([`4f310e6`](https://github.com/PulseBeamDev/pulsebeam/commit/4f310e6988af585f6532487c4096eaea5783f846))
    - Refactor controller and room ([`2d09fbd`](https://github.com/PulseBeamDev/pulsebeam/commit/2d09fbd54d45e6506bf9191139f56475bd5f4c84))
    - Remove crate rng and refactor participant ([`9d92aa8`](https://github.com/PulseBeamDev/pulsebeam/commit/9d92aa84048777b7021b22d649a7f0cb1a81d075))
    - Refactor track, room, participant more ([`a00fac6`](https://github.com/PulseBeamDev/pulsebeam/commit/a00fac612ba2343328e17322a8643c4ff2bf4204))
    - Rng -> rand runtime ([`09d67b4`](https://github.com/PulseBeamDev/pulsebeam/commit/09d67b4840295251c4628530750cfaef4028fe2e))
    - Introduce system_ctx and remove other hooks ([`ae8f8f4`](https://github.com/PulseBeamDev/pulsebeam/commit/ae8f8f449caec870882434680f897a9d02afab5c))
    - Add prelude and convert more actors ([`c64b2bb`](https://github.com/PulseBeamDev/pulsebeam/commit/c64b2bb2a8ba4bdd955d7798f788f922622e0142))
    - Replace some participant stuff ([`e2f029a`](https://github.com/PulseBeamDev/pulsebeam/commit/e2f029a59a17a8406c173fbfaf0d7c888aa591ae))
    - Refactor more dependencies ([`c03d99e`](https://github.com/PulseBeamDev/pulsebeam/commit/c03d99e3c616f56f1cdae2102237c6a75bf809cb))
    - Add ActorContext to runtime ([`7f15fab`](https://github.com/PulseBeamDev/pulsebeam/commit/7f15fabcb511f2e0e27d55d4fb5754a9975eba22))
    - Refactor source and sink ([`38747f3`](https://github.com/PulseBeamDev/pulsebeam/commit/38747f3bb83f802ed4cb8e369c08525660193eae))
    - Init runtime refactor ([`87b5b20`](https://github.com/PulseBeamDev/pulsebeam/commit/87b5b20766776e131aedad4df6cc160a48ea22ac))
    - Init net runtime ([`88b265e`](https://github.com/PulseBeamDev/pulsebeam/commit/88b265efb9158a557619fc056a50c8514bc32499))
    - Add Send to packetSocket ([`eb1e250`](https://github.com/PulseBeamDev/pulsebeam/commit/eb1e25048a2bd08346178248624afd7fc16ff38c))
    - Use stable and add instrument ([`9e86a1b`](https://github.com/PulseBeamDev/pulsebeam/commit/9e86a1b36d7f276c23a6b1e20268c04be42ba138))
    - Add more from original actor ([`e066011`](https://github.com/PulseBeamDev/pulsebeam/commit/e066011ed5a13345171cf114deddf01e43ec43e9))
    - Restructure and init minimal actor lib ([`4fba844`](https://github.com/PulseBeamDev/pulsebeam/commit/4fba844466ca29d24e3fdf98ea0987a16dc694a0))
</details>

