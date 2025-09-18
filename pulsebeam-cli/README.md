# `pulsebeam-cli`

**STATUS: PLANNED - NOT YET IMPLEMENTED**

This crate will be the primary command-line tool for interacting with, managing, and testing a remote PulseBeam SFU server.

## Purpose

This is the developer and operator's toolkit. It provides a consistent, scriptable interface for all interactions with a running SFU's control plane and for running load tests against its data plane.

**This crate DOES NOT run the SFU server.** It is exclusively a client application that communicates with a `pulsebeam` server instance over the network.

## Planned Commands

The vision is for a modular, `clap`-driven application with several key subcommands, all targeting a remote SFU.

#### `token`: Managing Access Tokens

For interacting with the SFU's control plane API to manage authentication and authorization.

```sh
# Create a new token for a participant in a specific room on the target SFU
pulsebeam-cli --sfu-url "https://sfu.example.com" token create --room "live-event" --participant-id "host-1"

# Revoke an existing token
pulsebeam-cli --sfu-url "https://sfu.example.com" token revoke <the-long-token-string>

# Inspect a token to see its claims
pulsebeam-cli token inspect <the-long-token-string>
```

#### `bench`: Running Benchmarks and Load Tests

To validate the performance and stability of an SFU by simulating real-world load. This command will use `pulsebeam-agent` under the hood.

```sh
# Run a benchmark with 50 virtual participants for 5 minutes against the target SFU
pulsebeam-cli --sfu-url "https://sfu.example.com" bench run --room "stress-test-1" --participants 50 --duration 300s

# Run a simpler "ping" test to check basic connectivity and session setup time
pulsebeam-cli --sfu-url "https://sfu.example.com" bench ping
```

#### `room`: Inspecting and Managing Live Rooms

For querying the state of live rooms on the SFU. This provides crucial visibility for debugging and monitoring.

```sh
# List all active rooms on the SFU
pulsebeam-cli --sfu-url "https://sfu.example.com" room list

# Inspect a specific room to see its participants and tracks
pulsebeam-cli --sfu-url "https://sfu.example.com" room inspect "live-event"
```

## Architectural Plan

-   **`clap`:** The CLI will be built using `clap` for robust argument parsing and help generation.
-   **Modular Subcommands:** Each command (`token`, `bench`, `room`) will be a self-contained module.
-   **Client-Centric Logic:**
    -   The `bench` command will instantiate and manage multiple `pulsebeam-agent` instances to create media sessions.
    -   The `token` and `room` commands will use a shared HTTP client to communicate with the SFU's control plane API.

## Current Status & Next Steps

This crate is currently a placeholder. No implementation exists yet.

The high-level implementation plan is:

1.  **Define the SFU Control Plane API:** The `token` and `room` commands depend on a well-defined HTTP API on the `pulsebeam` server. This API is the first prerequisite.
2.  **Setup `clap` Structure:** Implement the basic CLI structure with all planned subcommands and global options like `--sfu-url` as placeholders.
3.  **Implement Control Plane Commands:** Once the API is available on the server, implement the `token` and `room` commands and their underlying HTTP client logic.
4.  **Implement Benchmark Command:** This is the most complex step and depends on the `pulsebeam-agent` library being functional. This will be implemented last.
