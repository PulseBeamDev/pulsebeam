---
title: Deployment
description: Run PulseBeam in production behind TLS, from a single node to a horizontally scaled fleet.
---

# Deployment

PulseBeam is one binary with no external dependencies, so production deployment
is mostly about two things: terminating TLS in front of it, and — once you scale
past one node — routing each participant to the node that holds their room. This
page covers both.

## Deployment Architectures

### 1. Single Node (Quick Start)

Best for testing, demos, or low-traffic internal tools. Caddy handles SSL
termination and proxies signaling traffic to PulseBeam.

```mermaid
graph TD
    User((User)) -- "HTTPS (443)" --> Caddy[Caddy Proxy]

    subgraph Host ["Single VM / Docker Host"]
        Caddy -- "localhost:3000" --> PB[PulseBeam]
    end

```

```yaml
version: "3.8"
services:
  pulsebeam:
    image: ghcr.io/pulsebeamdev/pulsebeam:pulsebeam-v0.4.6
    restart: unless-stopped
    network_mode: "host"
    command:
      - --dev # this sets rtc port to 3478

  caddy:
    image: caddy:2
    restart: unless-stopped
    network_mode: "host"
    volumes:
      - caddy_data:/data
    command:
      - caddy
      - reverse-proxy
      - --from
      - api.example.com # <-- replace this with your domain
      - --to
      - localhost:3000

volumes:
  caddy_data:
```

### 2. Multi-Node (Production/High Availability)

For production environments requiring high availability, we recommend replacing
Caddy with HAProxy. The reverse proxy must be configured with consistent hashing
to ensure each participant connects to the correct node.

In the future, we will remove this limitation by allowing a single room to be
handled by multiple nodes.

```mermaid
graph TD
    %% Styling
    classDef haproxy fill:#fdfdfd,stroke:#005c94,stroke-width:2px;
    classDef sfu fill:#f9f9f9,stroke:#333,stroke-width:2px;

    Client((Client)) -- "HTTPS" --> CLB[Cloud Load Balancer]

    subgraph HA_Layer [HAProxy Fleet]
        HA1[HAProxy 1]
        HA2[HAProxy 2]
    end

    subgraph SFU_Fleet [PulseBeam SFU Fleet]
        direction LR
        PB1[Node A]
        PB2[Node B]
        PB3[Node C]
    end

    CLB --> HA1
    CLB --> HA2

    %% Logical routing
    HA1 -. "Hash(Room-ID)" .-> PB1
    HA2 -. "Hash(Room-ID)" .-> PB1
    HA1 -. "Hash(Room-ID)" .-> PB3

    class HA1,HA2 haproxy;
    class PB1,PB2,PB3 sfu;
```

## Port & Firewall Requirements

| Port     | Protocol | Traffic | Scope    | Purpose                           |
| -------- | -------- | ------- | -------- | --------------------------------- |
| **443**  | **UDP**  | Inbound | Public   | ICE over UDP                      |
| **443**  | **TCP**  | Inbound | Public   | ICE over TCP fallback             |
| **3000** | **TCP**  | Inbound | Internal | Proxied HTTP signaling            |
| **6060** | **TCP**  | Inbound | Private  | Metrics, health checks, and pprof |

> **Note on Networking:** `network_mode: "host"` is required to eliminate the
> overhead of Container’s network virtualization. For high-throughput real-time
> traffic, removing NAT indirection is critical for performance and ensures the
> SFU can correctly discover its public IP for ICE candidate gathering.
