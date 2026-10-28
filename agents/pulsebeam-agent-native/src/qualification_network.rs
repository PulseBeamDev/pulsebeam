//! Explicit real loopback enumeration for the owner qualification executable only.
use pulsebeam_webrtc_sys as rtc;
use std::net::{Ipv4Addr, Ipv6Addr};

pub(crate) struct LoopbackInterfaces;

struct Manager {
    signals: rtc::NetworkManagerSignals,
    networks: Vec<rtc::Network>,
}

impl rtc::NetworkManagerFactoryProvider for LoopbackInterfaces {
    fn create(&self, context: rtc::NetworkManagerContext) -> Option<Box<dyn rtc::NetworkManager>> {
        let mut v4 = rtc::Network::with_adapter(
            b"lo-v4",
            b"real loopback IPv4",
            Ipv4Addr::new(127, 0, 0, 0),
            8,
            rtc::NetworkAdapterType::Loopback,
        );
        v4.add_ip(Ipv4Addr::LOCALHOST);
        let mut v6 = rtc::Network::with_adapter(
            b"lo-v6",
            b"real loopback IPv6",
            Ipv6Addr::LOCALHOST,
            128,
            rtc::NetworkAdapterType::Loopback,
        );
        v6.add_ip(Ipv6Addr::LOCALHOST);
        Some(Box::new(Manager {
            signals: context.signals(),
            networks: vec![v4, v6],
        }))
    }
}

impl rtc::NetworkManager for Manager {
    fn start_updating(&self) {
        self.signals.notify_networks_changed();
    }

    fn stop_updating(&self) {}

    fn networks(&self) -> Vec<rtc::Network> {
        self.networks.iter().map(rtc::Network::share).collect()
    }

    fn any_address_networks(&self) -> Vec<rtc::Network> {
        Vec::new()
    }
}

pub(crate) fn configuration() -> rtc::RtcConfiguration {
    let mut configuration = rtc::RtcConfiguration::default();
    // Pinned native port_allocator.h: PORTALLOCATOR_ENABLE_IPV6.
    configuration.options.port_allocator.flags |= 1 << 6;
    configuration
}
