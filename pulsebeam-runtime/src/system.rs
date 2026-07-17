use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use systemstat::{IpAddr as SysIpAddr, Platform, System};

/// https://stackoverflow.com/questions/77585473/rust-tokio-how-to-handle-more-signals-than-just-sigint-i-e-sigquit
/// Waits for a signal that requests a graceful shutdown, like SIGTERM or SIGINT.
#[cfg(unix)]
pub async fn wait_for_signal_impl() {
    use tokio::signal::unix::{SignalKind, signal};

    // Infos here:
    // https://www.gnu.org/software/libc/manual/html_node/Termination-Signals.html
    let mut signal_terminate = signal(SignalKind::terminate()).unwrap();
    let mut signal_interrupt = signal(SignalKind::interrupt()).unwrap();

    tokio::select! {
        _ = signal_terminate.recv() => tracing::debug!("received SIGTERM."),
        _ = signal_interrupt.recv() => tracing::debug!("received SIGINT."),
    };
}

/// Waits for a signal that requests a graceful shutdown, Ctrl-C (SIGINT).
#[cfg(windows)]
pub async fn wait_for_signal_impl() {
    use tokio::signal::windows;

    // Infos here:
    // https://learn.microsoft.com/en-us/windows/console/handlerroutine
    let mut signal_c = windows::ctrl_c().unwrap();
    let mut signal_break = windows::ctrl_break().unwrap();
    let mut signal_close = windows::ctrl_close().unwrap();
    let mut signal_shutdown = windows::ctrl_shutdown().unwrap();

    tokio::select! {
        _ = signal_c.recv() => tracing::debug!("received CTRL_C."),
        _ = signal_break.recv() => tracing::debug!("received CTRL_BREAK."),
        _ = signal_close.recv() => tracing::debug!("received CTRL_CLOSE."),
        _ = signal_shutdown.recv() => tracing::debug!("received CTRL_SHUTDOWN."),
    };
}

/// Registers signal handlers and waits for a signal that
/// indicates a shutdown request.
pub async fn wait_for_signal() {
    wait_for_signal_impl().await
}

pub fn select_host_addresses() -> Vec<IpAddr> {
    #[derive(Default, Clone)]
    struct InterfaceCandidates {
        external_v4: Option<Ipv4Addr>,
        lan_v4: Option<Ipv4Addr>,
        external_v6: Option<Ipv6Addr>, // Global Unicast (e.g., 2001::)
        ula_v6: Option<Ipv6Addr>,      // Unique Local / LAN (fc00::/7)
        link_local_v6: Option<Ipv6Addr>, // Link-Local fallback (fe80::/10)
    }

    impl InterfaceCandidates {
        fn best_v4(&self) -> Option<Ipv4Addr> {
            self.external_v4.or(self.lan_v4)
        }

        fn best_v6(&self) -> Option<Ipv6Addr> {
            self.external_v6.or(self.ula_v6).or(self.link_local_v6)
        }

        // Rank 3 = Public/External, Rank 2 = Private/LAN/ULA, Rank 1 = Link-Local, 0 = Empty
        fn v4_rank(&self) -> u8 {
            if self.external_v4.is_some() {
                3
            } else if self.lan_v4.is_some() {
                2
            } else {
                0
            }
        }

        fn v6_rank(&self) -> u8 {
            if self.external_v6.is_some() {
                3
            } else if self.ula_v6.is_some() {
                2
            } else if self.link_local_v6.is_some() {
                1
            } else {
                0
            }
        }
    }

    let system = System::new();
    let networks = match system.networks() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("could not get network interfaces: {e}");
            return vec![
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ];
        }
    };

    let mut best_iface_name: Option<String> = None;
    let mut best_iface: Option<InterfaceCandidates> = None;

    for (name, net) in &networks {
        // Skip virtual/container management abstractions
        if name.starts_with("docker")
            || name.starts_with("veth")
            || name.starts_with("br-")
            || name.starts_with("virbr")
        {
            tracing::debug!("skipping virtual interface {}", name);
            continue;
        }

        let mut candidates = InterfaceCandidates::default();

        for n in &net.addrs {
            match n.addr {
                SysIpAddr::V4(ipv4) => {
                    if ipv4.is_loopback()
                        || ipv4.is_unspecified()
                        || ipv4.is_multicast()
                        || ipv4.is_link_local()
                    {
                        tracing::debug!("skipping loopback/unroutable v4 on {}: {}", name, ipv4);
                        continue;
                    }

                    if !ipv4.is_private() {
                        if candidates.external_v4.is_none() {
                            candidates.external_v4 = Some(ipv4);
                        }
                        tracing::info!("found candidate external ipv4 on {}: {}", name, ipv4);
                    } else {
                        if candidates.lan_v4.is_none() {
                            candidates.lan_v4 = Some(ipv4);
                        }
                        tracing::info!("found candidate lan ipv4 on {}: {}", name, ipv4);
                    }
                }
                SysIpAddr::V6(ipv6) => {
                    if ipv6.is_loopback() || ipv6.is_unspecified() || ipv6.is_multicast() {
                        tracing::debug!(
                            "skipping fundamental unroutable ipv6 on {}: {}",
                            name,
                            ipv6
                        );
                        continue;
                    }

                    if ipv6.is_unicast_link_local() {
                        if candidates.link_local_v6.is_none() {
                            candidates.link_local_v6 = Some(ipv6);
                        }
                        tracing::info!(
                            "found candidate link-local fallback ipv6 on {}: {}",
                            name,
                            ipv6
                        );
                    } else if ipv6.is_unique_local() {
                        if candidates.ula_v6.is_none() {
                            candidates.ula_v6 = Some(ipv6);
                        }
                        tracing::info!("found candidate lan ula ipv6 on {}: {}", name, ipv6);
                    } else {
                        if candidates.external_v6.is_none() {
                            candidates.external_v6 = Some(ipv6);
                        }
                        tracing::info!(
                            "found candidate external global ipv6 on {}: {}",
                            name,
                            ipv6
                        );
                    }
                }
                SysIpAddr::Empty | SysIpAddr::Unsupported => {
                    tracing::debug!("skipping unsupported interface address type on {}", name);
                }
            }
        }

        // An interface is valid if it possesses AT LEAST one usable address (v4 or v6)
        if candidates.best_v4().is_none() && candidates.best_v6().is_none() {
            continue;
        }

        // Compare using tuple comparison rules: V4 rank takes priority, V6 rank breaks ties.
        let replace_best = match &best_iface {
            None => true,
            Some(current) => {
                (candidates.v4_rank(), candidates.v6_rank())
                    > (current.v4_rank(), current.v6_rank())
            }
        };

        if replace_best {
            best_iface_name = Some(name.clone());
            best_iface = Some(candidates);
        }
    }

    if let Some(selected) = best_iface {
        let mut out = Vec::with_capacity(2);
        if let Some(v4) = selected.best_v4() {
            out.push(IpAddr::V4(v4));
        }
        if let Some(v6) = selected.best_v6() {
            out.push(IpAddr::V6(v6));
        }

        tracing::info!(
            iface = best_iface_name.unwrap_or_else(|| "<unknown>".to_string()),
            ?out,
            "selected interface host addresses dynamically"
        );
        return out;
    }

    tracing::warn!("no active network interfaces detected; returning local fallback anchors");
    vec![
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ]
}
