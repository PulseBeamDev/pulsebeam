---
title: Introduction
description: PulseBeam is an opinionated, open-source WebRTC SFU that trades flexibility for simplicity and stability.
---

# Introduction

PulseBeam provides a high-level, opinionated real-time media server that allows
you to build video, audio, and even generic real-time data (faster than
WebSocket) to production as fast as possible with minimal friction.

We use WebRTC as the underlying transport protocol. Unlike traditional WebRTC
deployments, we made conscious choices to reduce complexity and aim for the system
stability.

Here are a couple of notable choices (If you have feedback on this, you're very
welcome to reach us—we want to hear from you!):

1. SFU only
2. No WebSockets
3. No STUN & TURN Servers
4. No WebRTC Port Range
5. H264 and Opus codecs

## SFU Only

There are typically 3 foundational network architectures for WebRTC:

- **P2P:** Direct client-to-client. Simple, but bandwidth and the number of
  connections explode with more participants. Not ideal for group calls.
- **MCU:** Server mixes everyone’s streams. Easier for clients, but adds
  latency, heavy server load, and reduces flexibility.
- **SFU:** Server forwards streams without heavy processing. Clients handle
  rendering, so latency stays low, quality stays high, and it scales well with
  many participants.

PulseBeam chooses SFU only because it hits the sweet spot: scalable,
low-latency, high-quality real-time media, and low CPU usage.

## No WebSocket

We don't use WebSockets. Instead, we categorize WebRTC signaling into 2 separate
categories:

- **Connection (low-frequency):** join, leave, reconnect
- **Media (high-frequency):** media subscription, layout changes, audio level,
  stream quality

We use HTTP for connections and data channels for media. The rationale here is to
minimize the number of persistent connections from the client to the server.
After the WebRTC connection is established through HTTP, the client has a
persistent bidirectional connection directly to the server—we might as well use
It is for handling media signaling.

Aside from reducing the DevOps burden of configuring infra properly, data
channels allow much higher signaling frequency and lower per-message latency.
This also means a more responsive UI for the end-users.

## No STUN & TURN Servers

STUN servers are used to discover the public IP and port of clients. TURN
servers are used as a fallback in case no direct connection can be made.

We don't use STUN servers because PulseBeam is SFU only, and static
configuration at server startup is preferred to avoid potential network
failures. No TURN servers are needed because PulseBeam requires every deployment
to have a fixed, addressable IPv4 or IPv6 address. We also support direct TCP
connections.

While we want to say this is a novel approach, it is very similar to what Google
Meet has been in production for years. With no STUN and TURN servers,
air-gap deployment is straightforward: just run the service binary. No external
service is needed.

## No WebRTC Port Range

There's no 1:1 mapping between an OS UDP/TCP port and a WebRTC connection.
Instead, we use a combination of the 5-tuple (src_ip, src_port, dst_ip,
dst_port, proto) and WebRTC ICE metadata to multiplex a single port. Thus, we
only require 2 ports for WebRTC: 1 TCP port and 1 UDP port.

In theory, the server can serve much more than ~16k connections (the
typical limit from ephemeral UDP port exhaustion). This is useful for packing
more low-load connections like audio-only and/or data-channel-only. We haven't
done any benchmarks for this particular scenario yet—stay tuned!

## H264 and Opus Codecs

To reduce the "signaling dance," which can disrupt the end-user experience, we
want to avoid re-constructing media streams to different codecs in-flight. Thus,
we allow only 1 codec per media type per room. This trades off some bandwidth
efficiency compared to modern codecs in ideal cases, but we argue that most
production systems live in a less-than-ideal world where we have to support
clients with less hardware capability.

For audio, Opus won easily. It is prevalent and robust.

For video, the answer is complicated. There are H264, VP8, VP9, AV1 (of course,
there are H265, H266, AV2 too). But the decision comes down to compatibility.
H264 is chosen for now because it’s the only codec where you can actually count
on hardware acceleration being present on almost any device—from security
cameras to cheap TVs. By letting the hardware do the work, the device stays cool
, and the battery lasts longer. This ensures a smooth experience even on weaker
devices, rather than forcing a low-end CPU to struggle with software decoding.

## Next Steps

- **[Quickstart](/quickstart)** — run a server and stream your first
  video in a couple of minutes.
- **[Deployment](/deployment)** — take it to production with TLS and,
  when you need it, horizontal scaling.
