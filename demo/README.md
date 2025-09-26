# PulseBeam Demo

This example demonstrates a quick way to run PulseBeam with HTTPS using Caddy.

## Requirements

- Linux host (for host networking)
- Docker + Docker Compose **or** Podman + Podman Compose
- A public domain pointing to your host (e.g., `demo.pulsebeam.dev`)

## Quickstart

1. Copy the `docker-compose.yml` file.
2. Replace `YOUR_DOMAIN_HERE` in the compose file with your domain.
3. Run the stack:

```bash
podman-compose up -d
# or
docker-compose up -d
````

4. The server will automatically fetch a Let's Encrypt certificate for your domain.
5. Access the SFU:

* Signaling: `http://YOUR_DOMAIN_HERE:3000`
* WebRTC: `udp://YOUR_DOMAIN_HERE:3478`
