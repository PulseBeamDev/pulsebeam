# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/PulseBeamDev/pulsebeam/releases/tag/pulsebeam-runtime-v0.1.0) - 2025-09-10

### Other

- use release please token

## v0.1.0 (2025-09-11)

<csr-id-f5b95d1f0192d19731a1ec8846ff70059f75682b/>

### Chore

 - <csr-id-f5b95d1f0192d19731a1ec8846ff70059f75682b/> release v0.1.0

### New Features

 - <csr-id-cad7b8f249b54d63e203bc7f2e35fe24098d0a7c/> separate io and cpu runtimes

### Bug Fixes

 - <csr-id-37e49a5feff56268647c8b6a2ce23aaac4f550cc/> warnings and simplify audio selector
 - <csr-id-bd715935ca57b76a93a0c8dc791401f3f5c37f91/> sink buffer size and use flume
 - <csr-id-cba67ea2c5f0b59d82e9817567688a9dc3157a6c/> test using observable state

### Commit Statistics

<csr-read-only-do-not-edit/>

 - 39 commits contributed to the release over the course of 14 calendar days.
 - 5 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 0 issues like '(#ID)' were seen in commit messages

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **Uncategorized**
    - Release pulsebeam-runtime v0.1.0, pulsebeam v0.1.3 ([`9c73d5b`](https://github.com/PulseBeamDev/pulsebeam/commit/9c73d5bbe6b2a376895ad4f080ded4b1e822e163))
    - Merge pull request #1 from PulseBeamDev/release-plz-2025-09-10T17-40-26Z ([`896b23c`](https://github.com/PulseBeamDev/pulsebeam/commit/896b23c243113dd1a2a521dbc273597feb165a29))
    - Disable publish ([`2193397`](https://github.com/PulseBeamDev/pulsebeam/commit/2193397f2002bedcd7acd61eb46ce0755bf951ce))
    - Release v0.1.0 ([`f5b95d1`](https://github.com/PulseBeamDev/pulsebeam/commit/f5b95d1f0192d19731a1ec8846ff70059f75682b))
    - Warnings and simplify audio selector ([`37e49a5`](https://github.com/PulseBeamDev/pulsebeam/commit/37e49a5feff56268647c8b6a2ce23aaac4f550cc))
    - Create LICENSE ([`fb10cfd`](https://github.com/PulseBeamDev/pulsebeam/commit/fb10cfd846e58bf0573f14f9e3af241c64fed86d))
    - Separate io and cpu runtimes ([`cad7b8f`](https://github.com/PulseBeamDev/pulsebeam/commit/cad7b8f249b54d63e203bc7f2e35fe24098d0a7c))
    - Revert flume ([`8fceb90`](https://github.com/PulseBeamDev/pulsebeam/commit/8fceb90b045175f59bd40dbf56165dab6cb49be7))
    - Sink buffer size and use flume ([`bd71593`](https://github.com/PulseBeamDev/pulsebeam/commit/bd715935ca57b76a93a0c8dc791401f3f5c37f91))
    - Increase buffer size and add debug logs ([`5fa270b`](https://github.com/PulseBeamDev/pulsebeam/commit/5fa270b3ca53d9d29f462ad817d577af1dd69dc9))
    - Add actor id to tracing span ([`60e96b4`](https://github.com/PulseBeamDev/pulsebeam/commit/60e96b4c8325a464050d13a6a0fcaaec16190146))
    - Use actor_loop macro to implement ([`fe0cc14`](https://github.com/PulseBeamDev/pulsebeam/commit/fe0cc14fe0c76b057bd125e0f7b3f8c84817bd7f))
    - Init actor_loop macro ([`b011f76`](https://github.com/PulseBeamDev/pulsebeam/commit/b011f76ea244f9a09362c28bd90c51cabf362038))
    - Rename actor id to meta ([`bc14003`](https://github.com/PulseBeamDev/pulsebeam/commit/bc14003e6f1740888eb9c89f606eb061d745404b))
    - Add actor_id to handle ([`6da83fe`](https://github.com/PulseBeamDev/pulsebeam/commit/6da83febe06e9d84e9dcb96418817b65f537f008))
    - Test using observable state ([`cba67ea`](https://github.com/PulseBeamDev/pulsebeam/commit/cba67ea2c5f0b59d82e9817567688a9dc3157a6c))
    - Init spawn api instead ([`fb32908`](https://github.com/PulseBeamDev/pulsebeam/commit/fb32908f9569fe9fa58064947a67d06c40a79a4b))
    - Decouple state from actor for observability in testing ([`6076f5e`](https://github.com/PulseBeamDev/pulsebeam/commit/6076f5eef6c22f97eb5d4a267ab3399ec0eaf73a))
    - Init room test with simulation ([`ddf2d7a`](https://github.com/PulseBeamDev/pulsebeam/commit/ddf2d7a1ad4d94a940466e901faa5d6c593f77ab))
    - Init test stub ([`280b426`](https://github.com/PulseBeamDev/pulsebeam/commit/280b4266cb69b63378faffd7b56334097043f2c1))
    - More flexible actor trait ([`227873e`](https://github.com/PulseBeamDev/pulsebeam/commit/227873e7748e00067c468563cd032c774b4c438d))
    - Stub out main for now ([`21990a1`](https://github.com/PulseBeamDev/pulsebeam/commit/21990a12c8ce63bb0548935734e08ba23ae39bb1))
    - Use actorfactory prepare and relax trait constraints ([`9318088`](https://github.com/PulseBeamDev/pulsebeam/commit/9318088754dc9b31db0f2ffdc545c3f906ba8676))
    - Init actor factory ([`fc252be`](https://github.com/PulseBeamDev/pulsebeam/commit/fc252be397d6d94b3d84609187beb5584fb391dd))
    - Stub out interceptor ([`1939f1d`](https://github.com/PulseBeamDev/pulsebeam/commit/1939f1db4be6ec207bdeca05f66f346aca10f031))
    - Init interceptor ([`06b7f97`](https://github.com/PulseBeamDev/pulsebeam/commit/06b7f97473997a5a6dc882155553624877b46ee0))
    - Use runner concept ([`3a0c0f5`](https://github.com/PulseBeamDev/pulsebeam/commit/3a0c0f5bb79f5cd6d571c6cf73fc7de55491747f))
    - Compilable ([`c96dcaf`](https://github.com/PulseBeamDev/pulsebeam/commit/c96dcafe442cae75c3f816ce34af4f118a920835))
    - Add spawn config ([`4f310e6`](https://github.com/PulseBeamDev/pulsebeam/commit/4f310e6988af585f6532487c4096eaea5783f846))
    - Refactor controller and room ([`2d09fbd`](https://github.com/PulseBeamDev/pulsebeam/commit/2d09fbd54d45e6506bf9191139f56475bd5f4c84))
    - Rng -> rand runtime ([`09d67b4`](https://github.com/PulseBeamDev/pulsebeam/commit/09d67b4840295251c4628530750cfaef4028fe2e))
    - Introduce system_ctx and remove other hooks ([`ae8f8f4`](https://github.com/PulseBeamDev/pulsebeam/commit/ae8f8f449caec870882434680f897a9d02afab5c))
    - Add prelude and convert more actors ([`c64b2bb`](https://github.com/PulseBeamDev/pulsebeam/commit/c64b2bb2a8ba4bdd955d7798f788f922622e0142))
    - Replace some participant stuff ([`e2f029a`](https://github.com/PulseBeamDev/pulsebeam/commit/e2f029a59a17a8406c173fbfaf0d7c888aa591ae))
    - Add ActorContext to runtime ([`7f15fab`](https://github.com/PulseBeamDev/pulsebeam/commit/7f15fabcb511f2e0e27d55d4fb5754a9975eba22))
    - Refactor source and sink ([`38747f3`](https://github.com/PulseBeamDev/pulsebeam/commit/38747f3bb83f802ed4cb8e369c08525660193eae))
    - Init runtime refactor ([`87b5b20`](https://github.com/PulseBeamDev/pulsebeam/commit/87b5b20766776e131aedad4df6cc160a48ea22ac))
    - Add a simple proxy to UDP interface ([`860af39`](https://github.com/PulseBeamDev/pulsebeam/commit/860af39962702fb9d0c407e8562cf3e7ab4e94fe))
    - Init net runtime ([`88b265e`](https://github.com/PulseBeamDev/pulsebeam/commit/88b265efb9158a557619fc056a50c8514bc32499))
</details>

