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
    Fut: Future<Output = ()> + Send + 'static,
{
    #[cfg(feature = "sim")]
    {
        use std::sync::{Arc, Mutex};

        let test = Arc::new(Mutex::new(Some(test)));
        let mut sim = turmoil::Builder::new().build();
        sim.host(host_ip, {
            let test = Arc::clone(&test);
            move || {
                let test = Arc::clone(&test);
                async move {
                    let test = test
                        .lock()
                        .unwrap()
                        .take()
                        .expect("test future already used");
                    Box::pin(test).await;
                    Ok(())
                }
            }
        });
        sim.run().unwrap();
    }

    #[cfg(not(feature = "sim"))]
    {
        let _ = host_ip;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, test);
    }
}
