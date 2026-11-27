---
title: Deployment
description: Run a matching PulseBeam server and SDK behind TLS, with private health and metrics.
---

# Deploy PulseBeam

Start with one server using production authentication. Verify that two users can
exchange media before adding regional routes. Use a server image and SDK archives
built from the same revision. Do not substitute an older server image or assume
that registry `latest` is compatible with the current SDK.

## Run the server with production credentials

You need a Linux amd64 host with an address that clients can reach for ICE, Docker
with host networking, a DNS name for HTTPS signaling, a current IANA
`leap-seconds.list` from your host's time zone data package, and a matching server image.
Only pass `--unsmeared-utc` when the host time service actually uses unsmeared UTC.
If you are producing the image yourself, see [Runtime image](/releasing#runtime-image).

1. Generate a project API key and its public registry. The matching source CLI
   provides `./bazel run //:cli -- key`. Run it in a private terminal: its output
   includes the private API key. Store that key only in your application backend's
   secret store. Save only the public registry JSON as `key-registry.json`.
2. Set `PULSEBEAM_SERVER_IMAGE` to the matching image, preferably pinned by digest.
   On the server host, start PulseBeam without `--dev`:

   ```sh
   docker run --rm --name pulsebeam --network host \
     --mount "type=bind,src=$PWD/key-registry.json,dst=/app/key-registry.json,readonly" \
     --mount type=bind,src=/usr/share/zoneinfo/leap-seconds.list,dst=/app/leap-seconds.list,readonly \
     "$PULSEBEAM_SERVER_IMAGE" \
     --utc-policy /app/leap-seconds.list --http-listen 127.0.0.1:7070 --rtc-port 3478
   ```

   The image runs as UID/GID 65532. That user must be able to read both mounted
   files. If your time zone package installs the UTC policy at a different path,
   use that path as the mount source. The non-default RTC port avoids needing permission to bind port 443
   and leaves TCP 443 available for HTTPS. It does not enable development auth.
3. Put your TLS reverse proxy on the same host. For example, with DNS and HTTPS
   certificates managed by Caddy:

   ```caddyfile
   media.example.com {
       reverse_proxy 127.0.0.1:7070
   }
   ```

   Replace `media.example.com` with your DNS name. Proxy signaling only. ICE
   uses UDP 3478 directly, with passive ICE-TCP on TCP 3478. The address in each
   advertised candidate must be reachable at the advertised port. Use `--iface`
   to select an interface on a multihomed host. Do not assume a proxy, NAT, or
   port mapping rewrites candidates.

4. Check liveness on the host with `curl -fsS http://127.0.0.1:6060/healthz`.
   Then use the [React quickstart](/react/quickstart) with the matching SDK archives
   and `https://media.example.com` as the endpoint. Your application backend must
   authorize the user before signing a short-lived token. Grant `join` and
   `publish` only to publishers, and grant `subscribe` and, when needed, `discover`
   only for the rooms that viewer can access. Send the browser the token, never
   the signing key. Verify that a subscriber decodes the publisher's video. A
   successful `/healthz` alone does not verify signaling or media.

| Listener in this example | Access |
| --- | --- |
| TCP 443, HTTPS proxy | Clients |
| UDP and TCP 3478, ICE | Clients |
| TCP 7070, cleartext signaling | Loopback only |
| TCP 6060, health, metrics and profiling | Loopback only |

Do not proxy the internal listener. A private collector on another host needs an
explicit `--internal-listen` address and firewall rules that restrict it to the
collector. The internal routes do not authenticate callers.

## Qualify a regional topology before using it

Multi-node routing requires explicit provisioning. Gossip is an observation
service, not a source of authority, and is neither authenticated nor encrypted.
Put it on a trusted, access-restricted network. Keep regional control and WAN
media on dedicated authenticated adjacencies.

For an in-region protection rehearsal, put a publisher and a subscriber on
separate nodes. Place two relays in independent failure domains, each with a
route from publisher to subscriber that does not go through the other relay.
Make each link's interface, switch and uplink failure domains explicit. Two
logical routes on one shared uplink do not protect against loss of that uplink.

- Use `cluster.topology = "multi_node"` and `cluster.boot_admission = true`.
  Give each node a separate writable `--data-dir`. To allocate a new node ID,
  start once with `cluster.topology = "single_node"`, no peers, and matching
  loopback gossip listen and advertise addresses. Record the node ID from the
  membership startup log, then stop the node. Never copy or roll back this state.
- Give every node the same `[cluster.region_tree]` roster, including regions,
  gateway pairs, node IDs, roles, catalog flags and public keys. Set each
  member's `boot_key` to its receipt verification key. Use `media_key` only for
  members permitted to sign routed media. Roster generations and the local and
  peer generations on `[[cluster.adjacency]]` are enrollment minimums, not
  fixed active boots.
- Set fixed unicast IPv6 WAN endpoints, a common `authority_epoch`, the local
  `receipt_key_file`, and the `peer_receipt_key` for each adjacency. Do not set
  `certificate_der`, `key_pkcs8_der`, or `peer_sha256` in boot admission mode.
  `[cluster.adjacency.regional_control]` specifies the IPv6 TCP endpoints, roles,
  TLS name, CA, and peer certificate in DER form. Both sides check the exact TLS
  leaf; the dialing side also checks the TLS name. Restrict these ports to the
  configured peers. Keep private TLS and receipt keys separate from API keys.
- Set `--max-connections` per node from a load rehearsal of the deployed
  workload, including reconnect and single-node-loss headroom. It counts
  materializing and committed connections across projects and transports.
  At the limit, new materializations, including replacements, are rejected
  before allocating a media transport. The flag is optional and does not
  limit viewers per source or replace the directed-link budgets.
- Set `cluster.protected_base_stream_bps` for the subscribed base stream and
  `protected_egress_bps` independently for each qualified directed adjacency.
  Reserve enough capacity for the surviving route before accepting protected
  traffic. Do not infer a capacity limit from the configured values alone.
- Give every node a stable HTTPS origin. Keep a Native admission request and its
  returned `Location` on the issuing node. Recovery on another node requires a
  new admission ticket. Test with clients pinned to **different** origins; room
  hashing is not a substitute for discovery or authorization across nodes.

Before starting each node with this regional configuration, run the matching
server once while the node is offline:

```sh
pulsebeam --config /app/region.toml --data-dir /app/state --enroll-regional-boots
```

For the container, pass those arguments to the image used above, with the node's
configuration, credential files, and writable state directory mounted at the
configured paths. Enrollment refuses to overwrite existing floors. For normal
starts, omit `--enroll-regional-boots` and use the same configuration and state
directory. The peers negotiate a new DTLS pin after each restart. Do not rerun
enrollment or change the configured generations for a routine restart.

::: warning Qualify boot discovery and recovery in your topology
Non-neighbor discovery requires a qualified host clock. Configure the host's
unsmeared UTC discipline, current leap-second policy, and `--unsmeared-utc`.
An unqualified clock leaves remote targets unavailable for discovery. An unknown
catalog owner keeps results partial. Check actual decoding and route recovery
before enabling an unattended rollout. Do not erase either durable state file to
force recovery; missing or corrupt state requires explicit reprovisioning.
TLS and receipt-key changes still require coordinated pin changes.
:::

## Check protection and recover service

Scrape `/metrics` on the internal listener. `regional_control_boot_slots`
counts available and unavailable `direct` control lanes and `routed` boot slots.
If routed slots stay unavailable and `regional_boot_discovery_clock_qualified`
is zero, check the host clock and leap-second policy before investigating routes.
These gauges do not indicate that a media path is carrying traffic.

`regional_media_protection_sources` reports aggregate receiver-side source counts
by `state`. It covers resolved regional protection demand considered by the
receiver planner, not all sessions. `protected` means two fresh protected receive
legs, `degraded` means one leg remains from an established pair, and `unavailable`
means no fresh protected legs. Media can still play on an ordinary route when
protection is unavailable. Zero across all three can mean no eligible demand.
Always verify that the receiver decoded media as well as checking these counts.

- During a controlled rehearsal with eligible media playing, check that the
  source is protected before removing a route. Cut one independent path and
  confirm video continues on the survivor; cutting all paths is a recovery test,
  not a continuity test. Use receiver decode and the protection counts together.
- For workloads that require protection, investigate a persistent nonzero
  `degraded` or `unavailable` count. Check both routes, the authority views,
  configured directed budgets, and
  `fabric_wan_egress_peak_departure_queue_age_seconds`. Set sustained timing,
  queue and resource alerts from your measured envelope. Server queue time is
  not a receiver playout deadline.
- Before a drain, establish that the replacement is receiving fresh protected
  media and has headroom. Do not stop the process on the strength of liveness
  alone. If protection does not return, stop admitting new protected work and
  resolve the failed route or provisioning before continuing maintenance.
- During API key rotation, deploy a registry with both public keys before signing
  new tokens with the replacement. Remove the old key only after the overlap
  window. The registry is loaded at startup. For clustered registry updates,
  restart one node at a time and verify renewed peer admission and media before
  advancing. Do not roll back the durable node identity or peer floors. Use only
  image and configuration rollbacks that support the same state format.
- Keep incident captures access restricted. Correlate using scoped support
  references instead of capturing tokens, SDP, or media in metrics or alert
  labels. A healthy process with no decoded media calls for an end-to-end
  signaling, ICE and subscription check, not only a restart.
