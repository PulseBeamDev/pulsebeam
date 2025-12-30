use std::net::IpAddr;

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

pub fn select_host_address() -> IpAddr {
    let system = System::new();
    let networks = match system.networks() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("could not get network interfaces: {e}");
            return IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        }
    };

    let mut external_candidates = vec![];
    let mut lan_candidates = vec![];

    for (name, net) in &networks {
        // skip virtual / docker / bridge interfaces
        if name.starts_with("docker")
            || name.starts_with("veth")
            || name.starts_with("br-")
            || name.starts_with("virbr")
        {
            tracing::debug!("skipping virtual interface {}", name);
            continue;
        }

        // optionally restrict to known LAN interface patterns
        if !(name.starts_with("en") || name.starts_with("eth") || name.starts_with("wlp")) {
            tracing::debug!("skipping non-lan interface {}", name);
            continue;
        }

        for n in &net.addrs {
            if let SysIpAddr::V4(ipv4) = n.addr {
                if ipv4.is_loopback() {
                    tracing::debug!("skipping loopback {}: {}", name, ipv4);
                    continue;
                }

                if !ipv4.is_private() {
                    external_candidates.push(IpAddr::V4(ipv4));
                    tracing::info!("found candidate external ip on {}: {}", name, ipv4);
                } else {
                    lan_candidates.push(IpAddr::V4(ipv4));
                    tracing::info!("found candidate lan ip on {}: {}", name, ipv4);
                }
            }
        }
    }

    if let Some(ip) = external_candidates.first() {
        tracing::info!("selecting external ip: {}", ip);
        *ip
    } else if let Some(ip) = lan_candidates.first() {
        tracing::info!("selecting lan ip: {}", ip);
        *ip
    } else {
        tracing::warn!("falling back to localhost");
        IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    }
}
