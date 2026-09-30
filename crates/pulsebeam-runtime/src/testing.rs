#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::string_slice,
    clippy::indexing_slicing
)] // test / simulation support
use std::{future::Future, net::IpAddr};

pub fn test_host_ip(sim_host_ip: &str) -> IpAddr {
    #[cfg(feature = "sim")]
    {
        sim_host_ip.parse().expect("valid sim host ip")
    }

    #[cfg(not(feature = "sim"))]
    {
        use std::net::Ipv4Addr;

        let _ = sim_host_ip;
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }
}

pub fn run_local<Fut>(host_ip: IpAddr, test: Fut)
where
    Fut: Future<Output = ()> + 'static,
{
    #[cfg(feature = "sim")]
    {
        let mut sim = turmoil::Builder::new().build();
        sim.client(host_ip, async move {
            test.await;
            Ok(())
        });
        sim.run().unwrap();
    }

    #[cfg(not(feature = "sim"))]
    {
        let _ = host_ip;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|err| crate::fatal!("test runtime unavailable: {err}"));
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, test);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn local_future_completes_after_yielding() {
        let completed = Rc::new(Cell::new(false));
        let observed = Rc::clone(&completed);
        run_local(test_host_ip("192.168.250.13"), async move {
            tokio::task::yield_now().await;
            completed.set(true);
        });
        assert!(observed.get());
    }
}
