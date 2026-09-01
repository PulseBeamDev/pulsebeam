# PulseBeam Agent Native

Native runtime for the PulseBeam agent.

`agent-native` executes the SANS-I/O effects produced by `agent-core` using native platform facilities.

## Owns

* Native WebRTC integration
* Native media integration
* HTTP execution
* Timer execution
* Runtime and lifecycle plumbing
* Native resource handles
* UniFFI bindings for native environments

## Does not own

* Signaling policy
* Reconnection policy
* Topic protocol semantics
* Platform-independent agent behavior
* Application UI

```text
Rust / Swift / Kotlin / other hosts
              ↓
         agent-native
              ↓
          agent-core
```
