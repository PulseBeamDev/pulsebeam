use anyhow::{Context, Result, anyhow};
use aya::{
    Ebpf,
    maps::{HashMap, MapData, ReusePortSockArray},
    programs::SkReuseport,
};
use pulsebeam_routing::steer::{FlowKey, MAX_SHARDS};
use pulsebeam_runtime::net::BoundUdpSocket;
use std::{
    os::fd::AsFd,
    path::{Path, PathBuf},
};

pub(crate) struct Steering {
    _bpf: Ebpf,
    flows: HashMap<MapData, [u8; 40], u32>,
}

pub(crate) fn flow_key(source: std::net::SocketAddr, destination: std::net::SocketAddr) -> FlowKey {
    let (src_addr, src_v6) = socket_addr_parts(source);
    let (dst_addr, dst_v6) = socket_addr_parts(destination);
    debug_assert_eq!(src_v6, dst_v6, "a UDP flow cannot mix address families");
    FlowKey {
        src_addr,
        dst_addr,
        src_port: source.port(),
        dst_port: destination.port(),
        is_ipv6: u8::from(src_v6),
        _pad: [0; 3],
    }
}

fn socket_addr_parts(addr: std::net::SocketAddr) -> ([u8; 16], bool) {
    match addr.ip() {
        std::net::IpAddr::V4(ip) => {
            let mut bytes = [0; 16];
            bytes[..4].copy_from_slice(&ip.octets());
            (bytes, false)
        }
        std::net::IpAddr::V6(ip) => (ip.octets(), true),
    }
}

impl Steering {
    pub(crate) fn install_flow(&mut self, flow: FlowKey, shard: u16) -> Result<()> {
        if u32::from(shard) >= MAX_SHARDS {
            return Err(anyhow!("shard {shard} exceeds eBPF socket array"));
        }
        self.flows
            .insert(flow.to_ne_bytes(), u32::from(shard), 0)
            .context("installing authenticated UDP flow in FLOWS")
    }
}

pub(crate) fn attach(sockets: &[BoundUdpSocket]) -> Result<Option<Steering>> {
    if sockets.is_empty() {
        return Err(anyhow!("cannot attach UDP steering without sockets"));
    }
    let Some(path) = object_path()? else {
        tracing::warn!("eBPF object not found; using userspace bootstrap forwarding");
        metrics::gauge!("ebpf_steering_attached").set(0.0);
        return Ok(None);
    };

    let mut bpf = Ebpf::load_file(&path)
        .with_context(|| format!("loading eBPF object {}", path.display()))?;
    let mut sockarray: ReusePortSockArray<MapData> = bpf
        .take_map("SOCKARRAY")
        .ok_or_else(|| anyhow!("eBPF object has no SOCKARRAY map"))?
        .try_into()
        .context("opening SOCKARRAY map")?;
    for (index, socket) in sockets.iter().enumerate() {
        let index = u32::try_from(index).context("too many UDP workers for SOCKARRAY")?;
        let fd = socket.as_fd();
        sockarray
            .set(index, &fd, 0)
            .with_context(|| format!("populating SOCKARRAY[{index}]"))?;
    }
    let flows: HashMap<MapData, [u8; 40], u32> = bpf
        .take_map("FLOWS")
        .ok_or_else(|| anyhow!("eBPF object has no FLOWS map"))?
        .try_into()
        .context("opening FLOWS map")?;

    let program = bpf
        .program_mut("pulsebeam_client")
        .ok_or_else(|| anyhow!("eBPF object has no pulsebeam_client program"))?;
    let program: &mut SkReuseport = program.try_into().context("opening pulsebeam_client")?;
    program.load().context("loading pulsebeam_client")?;
    program
        .attach(sockets[0].as_fd())
        .context("attaching pulsebeam_client to SO_REUSEPORT")?;

    metrics::gauge!("ebpf_steering_attached").set(1.0);
    tracing::info!(path = %path.display(), workers = sockets.len(), "attached eBPF UDP steering");
    Ok(Some(Steering { _bpf: bpf, flows }))
}

fn object_path() -> Result<Option<PathBuf>> {
    if let Some(path) = std::env::var_os("PULSEBEAM_EBPF_OBJECT") {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(anyhow!(
                "PULSEBEAM_EBPF_OBJECT is not a file: {}",
                path.display()
            ));
        }
        return Ok(Some(path));
    }
    let path = Path::new("target/bpfel-unknown-none/release/pulsebeam-ebpf");
    Ok(path.is_file().then(|| path.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_runtime::net::{self, UdpMode};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[tokio::test]
    #[ignore = "requires a Linux kernel with eBPF and SO_REUSEPORT privileges"]
    async fn loader_attaches_to_a_reuseport_group() {
        let object = object_path()
            .expect("object path lookup must succeed")
            .expect("build the eBPF object before running the privileged smoke test");
        assert!(object.is_file());

        let first = net::bind_udp_socket(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            UdpMode::Batch,
            None,
            0,
        )
        .await
        .expect("first reuseport socket must bind");
        let address = first.local_addr();
        let second = net::bind_udp_socket(address, UdpMode::Batch, None, 1)
            .await
            .expect("second reuseport socket must join the group");

        let mut steering = attach(&[first, second])
            .expect("loading and attaching the eBPF object must succeed")
            .expect("the smoke test requires an object");
        steering
            .install_flow(flow_key("127.0.0.1:40000".parse().unwrap(), address), 1)
            .expect("authenticated flow must be writable");
    }
}
