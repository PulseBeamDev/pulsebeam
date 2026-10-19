use std::{
    cell::RefCell,
    net::{Ipv4Addr, SocketAddr},
    rc::Rc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use turmoil::exploration::{Decision, Event};
use turmoil::net::{TcpListener, TcpStream, UdpSocket};

fn host_order(control: Option<bool>) -> Vec<u8> {
    let log = Rc::new(RefCell::new(Vec::new()));
    let guard = control.map(|reverse| {
        turmoil::exploration::install(move |event| match event {
            Event::Hosts { .. } => Decision::Hosts { reverse },
            _ => Decision::Default,
        })
    });
    let mut sim = turmoil::Builder::new().rng_seed(7).build();
    for index in 1..=3 {
        let log = log.clone();
        sim.client(Ipv4Addr::new(10, 0, 0, index), async move {
            log.borrow_mut().push(index);
            Ok(())
        });
    }
    sim.run().unwrap();
    drop(sim);
    drop(guard);
    Rc::try_unwrap(log).unwrap().into_inner()
}

#[test]
fn host_choices_change_actual_runtime_order_and_guard_preserves_defaults() {
    assert_eq!(host_order(Some(false)), vec![1, 2, 3]);
    assert_eq!(host_order(Some(true)), vec![3, 2, 1]);
    let upstream = host_order(None);
    let guard = turmoil::exploration::install(|_| Decision::Default);
    assert_eq!(host_order(None), upstream);
    drop(guard);
    assert_eq!(host_order(None), upstream);
}

#[test]
fn udp_choices_change_real_delivery_and_delay_without_changing_bytes() {
    for mode in ["nominal", "reverse", "loss", "duplicate", "delay"] {
        let received = Rc::new(RefCell::new(Vec::new()));
        let output = received.clone();
        let delivered = Rc::new(RefCell::new(Vec::new()));
        let actual = delivered.clone();
        let guard = turmoil::exploration::install(move |event| match event {
            Event::Udp { bytes: [0], .. } => Decision::Udp {
                delay: if mode == "delay" {
                    Duration::from_millis(3)
                } else {
                    Duration::ZERO
                },
                drop: mode == "loss",
                duplicate: mode == "duplicate",
            },
            Event::UdpPair { .. } => Decision::UdpPair {
                reverse: mode == "reverse",
            },
            Event::UdpDelivered { bytes, .. } if bytes != [9] => {
                actual.borrow_mut().push(bytes[0]);
                Decision::Default
            }
            _ => Decision::Default,
        });
        let mut sim = turmoil::Builder::new()
            .rng_seed(7)
            .tick_duration(Duration::from_millis(1))
            .min_message_latency(Duration::from_millis(1))
            .max_message_latency(Duration::from_millis(1))
            .build();
        sim.client("receiver", async move {
            let _noise = UdpSocket::bind("0.0.0.0:5678").await?;
            let socket = UdpSocket::bind("0.0.0.0:1234").await?;
            let count = match mode {
                "loss" => 1,
                "duplicate" => 3,
                _ => 2,
            };
            for _ in 0..count {
                let mut bytes = [9];
                socket.recv_from(&mut bytes).await?;
                output.borrow_mut().push((bytes[0], turmoil::elapsed()));
            }
            Ok(())
        });
        sim.client("sender", async move {
            if mode == "reverse" {
                let noise = UdpSocket::bind("0.0.0.0:4322").await?;
                noise.send_to(&[9], "receiver:5678").await?;
            }
            let socket = UdpSocket::bind("0.0.0.0:4321").await?;
            socket.send_to(&[0], "receiver:1234").await?;
            socket.send_to(&[1], "receiver:1234").await?;
            Ok(())
        });
        sim.run().unwrap();
        drop(guard);
        let expected = match mode {
            "loss" => vec![1],
            "duplicate" => vec![0, 0, 1],
            "reverse" | "delay" => vec![1, 0],
            _ => vec![0, 1],
        };
        assert_eq!(*delivered.borrow(), expected, "{mode}");
        let received = received.borrow();
        assert_eq!(
            received.iter().map(|(value, _)| *value).collect::<Vec<_>>(),
            expected,
            "{mode}"
        );
        if mode == "delay" {
            assert!(received[1].1.saturating_sub(received[0].1) >= Duration::from_millis(3));
        }
    }
}

#[test]
fn udp_controls_do_not_reorder_or_duplicate_tcp_bytes() {
    let guard = turmoil::exploration::install(|event| match event {
        Event::Hosts { .. } => Decision::Hosts { reverse: true },
        Event::Udp { .. } => Decision::Udp {
            delay: Duration::from_millis(3),
            drop: false,
            duplicate: true,
        },
        Event::UdpPair { .. } => Decision::UdpPair { reverse: true },
        _ => Decision::Default,
    });
    let mut sim = turmoil::Builder::new().rng_seed(7).build();
    sim.client("listener", async {
        let udp = UdpSocket::bind("0.0.0.0:4321").await?;
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 1234))).await?;
        let (mut socket, _) = listener.accept().await?;
        let mut bytes = [0; 6];
        socket.read_exact(&mut bytes).await?;
        assert_eq!(&bytes, b"abcdef");
        let mut datagrams = Vec::new();
        for _ in 0..4 {
            let mut bytes = [9];
            udp.recv_from(&mut bytes).await?;
            datagrams.push(bytes[0]);
        }
        datagrams.sort();
        assert_eq!(datagrams, [0, 0, 1, 1]);
        Ok(())
    });
    sim.client("sender", async {
        let mut socket = TcpStream::connect("listener:1234").await?;
        let udp = UdpSocket::bind("0.0.0.0:4321").await?;
        socket.write_all(b"abc").await?;
        udp.send_to(&[0], "listener:4321").await?;
        socket.write_all(b"def").await?;
        udp.send_to(&[1], "listener:4321").await?;
        Ok(())
    });
    sim.run().unwrap();
    drop(guard);
}
