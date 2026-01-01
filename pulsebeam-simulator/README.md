# `pulsebeam-simulator`

Deterministic integration testing for the `pulsebeam` SFU and `pulsebeam-agent`.

## Purpose

This crate is our planned solution to eliminate test flakiness caused by real-world networking and timing. It will allow us to reliably reproduce and test complex, concurrent scenarios that are otherwise impossible to verify.

The goal is to test the interactions between the SFU and one or more clients in a fast, controlled, and 100% repeatable environment.

## Architecture Vision

The plan is to run the entire system (SFU and all agents) in-memory on a single thread. All I/O, including network and time, will be virtualized and controlled by a test framework.

```ascii
+-------------------------------------------------------------------------+
| Planned Simulated World (tokio-turmoil)                                 |
|                                                                         |
|  +-----------------------+     +------------------------------------+   |
|  |   Simulated Clock     |---->| All `tokio::time` calls (sleep,    |   |
|  | (Deterministic)       |     | timeout) will be controlled.       |   |
|  +-----------------------+     +------------------------------------+   |
|                                                                         |
|  +-----------------------+                                              |
|  |  Simulated Network    |<------------------------------------------+  |
|  | (In-Memory TCP/UDP)   |                                           |  |
|  +-----------------------+                                           |  |
|      ^           ^           ^           ^                           |  |
|      |           |           |           | (All I/O will be          |  |
|      |           |           |           |    virtualized)           |  |
|  +---+-----------+---+-------+-----------+---------------------------+  |
|  |                   |                   |                           |  |
|  v                   v                   v                           v  |
| +-----------------+ +-----------------+ +-----------------+         +--+---------------+
| | PulseBeam Agent | | PulseBeam Agent | | PulseBeam Agent | .....   | PulseBeam SFU    |
| | (on Host "a")   | | (on Host "b")   | | (on Host "c")   |         | (on Host "sfu")  |
| +-----------------+ +-----------------+ +-----------------+         +------------------+
|                                                                         |
+-------------------------------------------------------------------------+
```

## Core Mechanics (The Plan)

- **`tokio-turmoil`:** We will use `tokio-turmoil` to provide a deterministic `tokio` runtime. It will replace the real scheduler, clock, and network with virtual counterparts.
- **Simulated Clock:** Time will only advance when the simulation allows it. `tokio::time::sleep` will pause a task until the simulation clock has advanced enough. This will eliminate timing-related race conditions.
- **Simulated Network:** All network calls (`TcpListener`, `UdpSocket`, etc.) will be intercepted. Packets will be routed through an in-memory buffer, not the OS network stack.

## Developer Benefits (The Goal)

- **Reproducible Failures:** A failing test will fail **identically every time**. The logs and execution trace will be the same, making debugging trivial.
- **Safe Refactoring:** We will be able to refactor complex concurrent logic with high confidence. If the simulations pass, the change is likely correct.
- **Test Critical Edge Cases:** We will be able to reliably script and test scenarios like two clients publishing at the exact same logical time.
- **Fast Execution:** The entire test suite will run in-memory and complete in seconds.

## Current Status & Next Steps

This crate is currently a placeholder. No tests have been written, and the simulation infrastructure has not been implemented.
