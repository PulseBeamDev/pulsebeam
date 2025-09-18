# `pulsebeam-runtime`

This crate provides the core actor runtime for the PulseBeam ecosystem. It is a minimal, performance-focused framework for building concurrent, stateful services like the SFU.

It is implemented and currently in use by the `pulsebeam` server.

## Philosophy & Objectives

Standard `async` Rust provides concurrency primitives, but building complex, high-throughput systems requires a more structured approach. This runtime was created to solve specific problems encountered when building real-time media servers.

-   **High Throughput & Low Contention:** The primary goal is performance. The design prioritizes minimizing channel contention and keeping the hot path for data messages as fast as possible.
-   **Fault Isolation:** Actors are structured in a parent-child hierarchy. The lifetime of a child actor is tied to its parent, enabling supervision and preventing orphaned tasks.
-   **Clear Concurrency:** The system avoids a "sea of actors." Communication is explicit via handles, making the flow of data and control easy to reason about and debug.
-   **Control & Testability:** We provide control over channel capacity and expose actor handles that can be used directly in tests, facilitating robust integration testing.

## Core Concepts

This is not a traditional, pure actor system. It makes specific design choices to favor performance and clarity over dogmatic adherence to the actor model.

#### Structured Concurrency, Not a Global Bus

There is no global actor registry or address book. Actors exist in a strict ownership hierarchy.

-   An actor is created using `actor::spawn()`, which returns a handle to the new child actor and a `JoinHandle` for its task.
-   The parent actor is responsible for the lifetime of its children.
-   To communicate with an actor, you **must** have been given its `ActorHandle`.

#### Dual-Channel Priority Queues

Every actor has two distinct incoming message channels to prevent priority inversion:

1.  **`HighPriorityMsg`:** For urgent control messages (e.g., "disconnect", "state update", "configuration change"). These must be processed immediately, regardless of the data load.
2.  **`LowPriorityMsg`:** For high-volume data messages (e.g., media packets, telemetry).

This ensures that a flood of media packets can never block a critical disconnect command.

## Key Components

-   `trait actor::Actor`: The trait you implement to define your actor's state, message types, and `run` loop logic.
-   `struct actor::ActorHandle`: A cheap-to-clone, thread-safe handle for sending messages to a running actor. This is the primary API for interacting with an actor.
-   `fn actor::spawn`: The function used to create and start a new actor, tying it to the current runtime.
-   `macro actor_loop!`: An optional but recommended macro that simplifies the main actor `run` loop, handling message reception and polling logic.

## Roadmap & Performance Notes

The current implementation uses MPSC channels, which are known to be a point of contention under heavy load with multiple producers.

-   **TODO:** To further reduce contention, the plan is to replace the underlying MPSC channels with a multi-producer/multi-consumer strategy built on top of multiple Single-Producer/Single-Consumer (SPSC) channels. This is a known pattern for significantly improving throughput in high-core-count scenarios.
    -   See issue for discussion: [https://github.com/zesterer/flume/issues/152](https://github.com/zesterer/flume/issues/152)
