use super::*;
use crate::{id::ShardId, route::TransportRoute};
#[cfg(feature = "sim")]
use tokio::io::AsyncWriteExt;

fn address(epoch: u16) -> NodeTransportAddress {
    NodeTransportAddress::new(TransportRoute::new(ShardId::new(0), 7), epoch)
}

fn stun(address: NodeTransportAddress) -> Vec<u8> {
    let server = IceUfrag::new(0, 0, address.route, address.epoch).encode();
    let value = format!("{server}:client");
    let padded = (value.len() + 3) & !3;
    let mut packet = Vec::new();
    packet.extend_from_slice(&[0, 1]);
    packet.extend_from_slice(&u16::try_from(4 + padded).unwrap().to_be_bytes());
    packet.extend_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
    packet.extend_from_slice(&[0; 12]);
    packet.extend_from_slice(&[0, 6]);
    packet.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
    packet.extend_from_slice(value.as_bytes());
    packet.resize(24 + padded, 0);
    packet
}

fn host() -> IpAddr {
    pulsebeam_runtime::testing::test_host_ip("192.168.250.101")
}

async fn start(limits: Limits) -> (Service, SocketAddr, Inbox) {
    let (sender, inbox) = inbox(limits.inbox_capacity, limits.max_frame_bytes).unwrap();
    let (service, bound) = Service::start(
        "127.0.0.1:0".parse().unwrap(),
        vec![],
        Routing {
            cluster_id: 0,
            node_id: 0,
            shard_count: 1,
        },
        limits,
        vec![sender],
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    (service, bound, inbox)
}

#[test]
fn first_stun_is_delivered_once_and_ack_fences_following_frames() {
    pulsebeam_runtime::testing::run_local(host(), async {
        let limits = Limits {
            max_frame_bytes: 128,
            inbox_capacity: 2,
            stalled_io_timeout: Duration::from_millis(50),
            ..Limits::default()
        };
        let (service, bound, mut inbox) = start(limits).await;
        let client = TcpStream::connect(bound).await.unwrap();
        let (mut read, mut write) = pulsebeam_tcp::split(client, 128).unwrap();
        let binding = stun(address(1));
        write.send(&[]).await.unwrap();
        write.send(&binding).await.unwrap();
        write.send(b"next").await.unwrap();
        let Some(TcpIngress::Adopt {
            flow,
            first_stun,
            reply,
        }) = inbox.receiver.recv().await
        else {
            panic!("missing adoption");
        };
        assert_eq!(first_stun.packet, binding);
        assert_eq!(first_stun.route.address, address(1));
        let route = flow.route;
        drop(first_stun);
        assert!(
            tokio::time::timeout(Duration::from_millis(1), inbox.receiver.recv())
                .await
                .is_err()
        );
        reply.send(true).unwrap();
        let Some(TcpIngress::Frame(packet)) = inbox.receiver.recv().await else {
            panic!("missing following frame");
        };
        assert_eq!(packet.route, route);
        assert_eq!(packet.packet, b"next");
        drop(packet);
        // An idle authenticated flow is not a stalled partial frame.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(flow.is_open());
        assert!(flow.try_send(b"reply"));
        assert_eq!(&read.recv().await.unwrap().unwrap()[..], b"reply");
        drop(flow);
        assert!(
            matches!(inbox.receiver.recv().await, Some(TcpIngress::Closed { route: closed }) if closed == route)
        );
        service.finish().await.unwrap();
    });
}

#[test]
fn frame_quota_follows_deferred_input_and_egress_full_retires_flow() {
    pulsebeam_runtime::testing::run_local(host(), async {
        let local = SocketAddr::new(host(), 6000);
        let remote = SocketAddr::new(host(), 5000);
        let (flow, _outgoing) = fixture(address(1), local, remote, 1);
        let route = flow.route;
        let (sender, mut inbox) = inbox(1, 128).unwrap();
        let packet = sender
            .packet(route, b"pending", ReceiveTime::now())
            .unwrap();
        assert!(sender.channel.try_send(TcpIngress::Frame(packet)).is_ok());
        let Some(TcpIngress::Frame(packet)) = inbox.receiver.recv().await else {
            panic!("missing packet");
        };
        assert!(
            sender
                .packet(route, b"overflow", ReceiveTime::now())
                .is_err()
        );
        let (accepted, _, _) = crate::control::application_only_test_connection_and_binding();
        let mut transport = crate::participant::transport::Transport::new(
            accepted,
            8,
            tokio::time::Instant::now(),
            #[cfg(feature = "sim")]
            tracing::Span::none(),
        );
        assert!(transport.bind_tcp_flow(flow));
        assert!(transport.enqueue_tcp(packet));
        // Removing an inbox item does not free its frame quota before RTC consumes it.
        assert!(
            sender
                .packet(route, b"overflow", ReceiveTime::now())
                .is_err()
        );
        transport.receive_pending(tokio::time::Instant::now());
        assert!(
            sender
                .packet(route, b"released", ReceiveTime::now())
                .is_ok()
        );
        let (flow, _outgoing) = fixture(address(1), local, remote, 1);
        assert!(flow.try_send(b"one"));
        assert!(!flow.try_send(b"two"));
        assert!(!flow.is_open());
        assert!(!flow.try_send(b"three"));
    });
}

#[test]
fn full_inbox_releases_socket_before_fifo_close_and_shutdown_cancels_notice_wait() {
    pulsebeam_runtime::testing::run_local(host(), async {
        for shutdown_while_full in [false, true] {
            let limits = Limits {
                max_frame_bytes: 128,
                inbox_capacity: 2,
                ..Limits::default()
            };
            let (service, bound, mut inbox) = start(limits).await;
            let client = TcpStream::connect(bound).await.unwrap();
            let (mut read, mut write) = pulsebeam_tcp::split(client, 128).unwrap();
            write.send(&stun(address(1))).await.unwrap();
            let Some(TcpIngress::Adopt {
                flow,
                first_stun,
                reply,
            }) = inbox.receiver.recv().await
            else {
                panic!("missing adoption");
            };
            let route = flow.route;
            drop(first_stun);
            reply.send(true).unwrap();
            for packet in [b"one".as_slice(), b"two", b"overflow"] {
                write.send(packet).await.unwrap();
            }
            // No consumer space is available for Closed yet, but the socket is already retired.
            assert!(matches!(read.recv().await, Ok(None) | Err(_)));
            assert!(!flow.is_open());
            if shutdown_while_full {
                service.finish().await.unwrap();
            } else {
                for expected in [b"one".as_slice(), b"two"] {
                    let Some(TcpIngress::Frame(packet)) = inbox.receiver.recv().await else {
                        panic!("missing accepted frame");
                    };
                    assert_eq!(packet.route, route);
                    assert_eq!(packet.packet, expected);
                }
                assert!(
                    matches!(inbox.receiver.recv().await, Some(TcpIngress::Closed { route: closed }) if closed == route)
                );
                service.finish().await.unwrap();
            }
        }
    });
}

#[test]
fn admission_caps_and_first_stun_deadline_release_capacity() {
    pulsebeam_runtime::testing::run_local(host(), async {
        for (pending_cap, ip_cap, active_cap) in [(1, 8, 8), (8, 1, 8), (8, 8, 1)] {
            let limits = Limits {
                max_pending: pending_cap,
                max_per_ip: ip_cap,
                max_connections: active_cap,
                first_stun_timeout: Duration::from_secs(1),
                max_frame_bytes: 128,
                ..Limits::default()
            };
            let (service, bound, mut inbox) = start(limits).await;
            let client = TcpStream::connect(bound).await.unwrap();
            let (mut first_read, mut first_write) = pulsebeam_tcp::split(client, 128).unwrap();
            let flow = if active_cap == 1 {
                first_write.send(&stun(address(1))).await.unwrap();
                let Some(TcpIngress::Adopt {
                    flow,
                    first_stun,
                    reply,
                }) = inbox.receiver.recv().await
                else {
                    panic!("missing adoption");
                };
                drop(first_stun);
                reply.send(true).unwrap();
                Some(flow)
            } else {
                // Keep one connection pending, without a complete first STUN.
                first_write.send(&[]).await.unwrap();
                None
            };
            let second = TcpStream::connect(bound).await.unwrap();
            let (mut second_read, mut second_write) = pulsebeam_tcp::split(second, 128).unwrap();
            let _ = second_write.send(&stun(address(1))).await;
            assert!(matches!(second_read.recv().await, Ok(None) | Err(_)));
            if active_cap == 1 {
                let route = flow.as_ref().unwrap().route;
                drop(flow);
                assert!(
                    matches!(inbox.receiver.recv().await, Some(TcpIngress::Closed { route: closed }) if closed == route)
                );
            } else {
                assert!(matches!(first_read.recv().await, Ok(None) | Err(_)));
            }
            let third = TcpStream::connect(bound).await.unwrap();
            let (_, mut third_write) = pulsebeam_tcp::split(third, 128).unwrap();
            third_write.send(&stun(address(2))).await.unwrap();
            let Some(TcpIngress::Adopt {
                first_stun,
                reply,
                flow,
            }) = inbox.receiver.recv().await
            else {
                panic!("capacity was not released");
            };
            assert_eq!(flow.route.address, address(2));
            drop(first_stun);
            reply.send(true).unwrap();
            drop(flow);
            service.finish().await.unwrap();
        }
    });
}

#[test]
fn routing_hint_and_pre_auth_nulls_are_untrusted_and_bounded() {
    pulsebeam_runtime::testing::run_local(host(), async {
        let limits = Limits {
            max_frame_bytes: 128,
            ..Limits::default()
        };
        let (service, bound, mut inbox) = start(limits).await;
        let routing = Routing {
            cluster_id: 0,
            node_id: 0,
            shard_count: 1,
        };
        let valid = stun(address(1));
        assert_eq!(routing.hint(&valid), Some(address(1)));
        assert!(
            Routing {
                cluster_id: 1,
                ..routing
            }
            .hint(&valid)
            .is_none()
        );
        assert!(
            Routing {
                node_id: 1,
                ..routing
            }
            .hint(&valid)
            .is_none()
        );
        assert!(
            Routing {
                shard_count: 0,
                ..routing
            }
            .hint(&valid)
            .is_none()
        );
        for excess_nulls in [false, true] {
            let client = TcpStream::connect(bound).await.unwrap();
            let (mut read, mut write) = pulsebeam_tcp::split(client, 128).unwrap();
            if excess_nulls {
                for _ in 0..9 {
                    let _ = write.send(&[]).await;
                }
            } else {
                write.send(b"not STUN").await.unwrap();
            }
            assert!(matches!(read.recv().await, Ok(None) | Err(_)));
            assert!(inbox.receiver.try_recv().is_err());
        }
        service.finish().await.unwrap();
    });
}

#[cfg(feature = "sim")]
#[test]
fn stalled_partial_reader_and_writer_close_with_virtual_time() {
    let mut sim = turmoil::Builder::new().tcp_capacity(1).build();
    sim.client(host(), async {
        for stalled_writer in [false, true] {
            let limits = Limits {
                max_frame_bytes: usize::from(u16::MAX),
                stalled_io_timeout: Duration::from_millis(50),
                ..Limits::default()
            };
            let (service, bound, mut inbox) = start(limits).await;
            let mut client = TcpStream::connect(bound).await.unwrap();
            let binding = stun(address(1));
            client.write_all(&u16::try_from(binding.len()).unwrap().to_be_bytes()).await.unwrap();
            client.write_all(&binding).await.unwrap();
            let Some(TcpIngress::Adopt { flow, first_stun, reply }) = inbox.receiver.recv().await else { panic!("missing adoption"); };
            let route = flow.route;
            drop(first_stun);
            reply.send(true).unwrap();
            if stalled_writer {
                assert!(flow.try_send(&vec![1; usize::from(u16::MAX)]));
            } else {
                client.write_all(&[0]).await.unwrap();
            }
            assert!(matches!(inbox.receiver.recv().await, Some(TcpIngress::Closed { route: closed }) if closed == route));
            assert!(!flow.is_open());
            service.finish().await.unwrap();
        }
        Ok(())
    });
    sim.run().unwrap();
}
