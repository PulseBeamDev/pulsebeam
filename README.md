# PulseBeam FOSS Server

> [!WARNING]
> This SDK is currently in **Developer Preview**. During this phase:
> - APIs are subject to breaking changes
> - Stability issues may occur
> - Core functionality is still being validated
>
> **We value your input!**  
> Report bugs, suggest improvements, or collaborate directly with our team:
> 
> • [Create GitHub Issues](https://github.com/PulseBeamDev/pulsebeam-server-foss/issues)  
> • [Join PulseBeam Developer Discord](https://discord.gg/Bhd3t9afuB)  

## How to start a development server

`RUST_LOG='tower_http=trace,pulsebeam_server_foss=info' cargo run`

## How to build for production?

`cargo build --release`

## How to run with docker?

`docker run --rm -p 3000:3000 ghcr.io/pulsebeamdev/pulsebeam-server-foss:main`

## How to run a demo with the local server?

https://meet.pulsebeam.dev/?baseUrl=http://localhost:3000/twirp

## Semantic Versioning

This project adheres to [Semantic Versioning 2.0.0](https://semver.org/).

* **MAJOR version (X.y.z):** Incompatible API changes.
* **MINOR version (x.Y.z):** Functionality added in a backwards compatible manner.
* **PATCH version (x.y.Z):** Backwards compatible bug fixes.
