# `pulsebeam-agent`

**STATUS: IN_PROGRESS**

This crate will be the canonical WebRTC peer implementation for the PulseBeam ecosystem.

This is a library, not a binary. Its planned job is to handle the low-level complexities of a full WebRTC session, providing a clean, high-level API for an application to use. It will be the foundational client component for the entire PulseBeam stack.

## System Dependencies

This crate relies on ffmpeg to be available on the system. The final binary will be dynamically linked to ffmpeg.

Fedora:

```bash
sudo dnf install ffmpeg-free-devel clang-devel
```


## Planned Responsibilities

-   **Signaling (WHIP/WHEP Superset):** It will manage the entire media session lifecycle using our HTTP-based protocol. Instead of a persistent WebSocket, it will use a series of HTTP requests (POST, PATCH, DELETE) to negotiate SDP offers/answers.
-   **WebRTC Engine:** It will own and drive the `str0m::Rtc` state machine, handling all ICE, DTLS, and SRTP session logic.
-   **Track Management:** It will provide a clean API for publishing local media tracks and subscribing to remote tracks.
-   **Async Runtime Agnostic:** adapters will be provided as optional dependencies.

## Architectural Vision

The `Agent` will be an actor that encapsulates all state and I/O (networking, timers). This design prevents race conditions and simplifies state management.

-   **`Agent`:** The internal actor struct. It will own the `str0m::Rtc` instance and run the main event loop, handling the sequence of HTTP requests required for signaling.
-   **`AgentHandle`:** The public API. This will be a thread-safe handle used to send commands to the agent (e.g., `connect`, `publish_track`) and receive events from it (e.g., `TrackAdded`, `Disconnected`).

This model will clearly separate the public API from the internal implementation details of the HTTP-based signaling and the WebRTC state machine.

## Intended Role in the Ecosystem

This crate is designed to be a core, reusable component.

-   **`pulsebeam-cli`:** Will use this agent to provide a headless test client.

-   **`pulsebeam-simulator`:** This agent's logic will run directly inside the deterministic simulation.

-   **Native Applications:** Can be embedded directly into native Rust applications to provide WebRTC functionality without a WebView.

-   **Backend Services:** Can be used server-side for services that need to act as a WebRTC peer (e.g., recording bots, gateways).

## Current Status & Next Steps

This crate is currently a placeholder. No implementation exists yet.

The high-level implementation plan is:

1.  **Define the Public API:** Finalize the commands and events for the `AgentHandle`. This is the contract for all consumers of the agent.
2.  **Implement the Core Actor:** Create the `Agent` actor struct and the `run` loop skeleton.
3.  **Build the Signaling Client:** Implement the HTTP client logic for the WHIP/WHEP superset protocol.
4.  **Integrate the WebRTC Engine:** Wire the `str0m::Rtc` engine into the actor, connecting the signaling client's SDPs to the engine's state machine.
