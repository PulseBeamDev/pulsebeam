
<p align="center">
  <a href="https://pulsebeam.dev/">
    <img src="https://pulsebeam.dev/favicon.svg" width="140px" alt="PulseBeam" />
  </a>
</p>

# PulseBeam
Simple, Open-source, Fully Managed Cloud or Self-hostable WebRTC SFU 



## Architecture Overview

This project implements a WebRTC Selective Forwarding Unit (SFU) in Rust, utilizing the Tokio asynchronous runtime and its multi-threaded work-stealing scheduler. The design prioritizes performance and concurrency management through specific architectural choices.

### Design Goals & Trade-offs:

*   **Performance & Low Latency:** By minimizing application-level contention in the media path and leveraging asynchronous I/O, we aim for high throughput and low latency.
*   **Scalability:** The message-passing architecture is intended to distribute work effectively, allowing the SFU to scale with available CPU cores.
*   **Concurrency Management:** The actor-like model and message passing help manage concurrent operations. However, debugging and ensuring correctness in highly concurrent, message-driven systems require diligent design and testing strategies (like our simulation testing).
*   **Maintainability:** Isolating logic into components can improve modularity. However, understanding the flow of messages and overall system state can present its own complexities compared to monolithic designs.


### Core Architectural Approaches:

*   **Message-Driven Components (Actor-Inspired):**
    The system is structured around independent components (actors) that manage their own state and logic. These components communicate exclusively by sending messages through Tokio's `mpsc` (multi-producer, single-consumer) channels.
    *   **Benefit:** This isolates state, reducing the need for direct shared memory access and traditional locking mechanisms (like `Mutex` or `RwLock`) *within component logic*. It simplifies reasoning about individual component behavior.
    *   **Consideration:** While individual components are isolated, coordinating state or ensuring synchronized actions across multiple components requires careful message design and handling. To address the complexity of these distributed interactions, we employ **deterministic simulation testing** to rigorously verify system behavior under various scenarios.

*   **Optimized Media Pipeline via Channels:**
    The critical path for forwarding media packets (RTP/RTCP) is designed to minimize contention. Media data flows through dedicated `mpsc` channels.
    *   **How it works:** Instead of shared data structures protected by application-level locks, media packets are passed between stages of the pipeline via these channels.
    *   **Benefit:** This approach means our core media forwarding logic itself does not introduce explicit locks. While the underlying `mpsc` channels handle their own internal synchronization, this is abstracted away from the media processing path, aiming to reduce application-induced latency, jitter, and contention points.
    *   **Clarification:** The term "lock-free" here refers to the absence of *application-managed locks* directly in the media forwarding path, not an absence of all synchronization primitives at the lowest levels of the runtime or OS.

## Running

`RUST_LOG=info cargo run`

## Testing

Unit tests:

`cargo test --lib`

Deterministic Simulation Testing (dst), think of this like a [Chaos Monkey](https://github.com/Netflix/chaosmonkey) but running in a simulation:

`cargo test --test sim`
