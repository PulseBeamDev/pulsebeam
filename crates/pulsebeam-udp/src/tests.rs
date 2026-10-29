use super::*;

fn entry(payload: &[u8], segment: Option<u16>) -> SendDatagram<'_> {
    SendDatagram {
        destination: "127.0.0.1:9".parse().unwrap(),
        payload,
        gso_segment_size: segment.and_then(NonZeroU16::new),
    }
}

#[test]
fn submission_stops_at_exact_prefix_for_short_blocked_and_fatal_results() {
    let entries = [
        entry(b"abc", None),
        entry(b"def", None),
        entry(b"ghi", Some(2)),
    ];
    for count in [0, 1] {
        let mut calls = 0;
        let report = submit_prefix(&entries, BATCH_SIZE, |group| {
            calls += 1;
            assert_eq!(group.len(), 2);
            Ok(count)
        });
        assert_eq!(report.accepted, count);
        assert!(matches!(report.stop, Some(SendStop::Blocked)));
        assert_eq!(calls, 1);
    }
    for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::PermissionDenied] {
        let mut calls = 0;
        let report = submit_prefix(&entries, BATCH_SIZE, |group| {
            calls += 1;
            if calls == 1 {
                Ok(group.len())
            } else {
                Err(io::Error::from(kind))
            }
        });
        assert_eq!(report.accepted, 2);
        match report.stop {
            Some(SendStop::Blocked) => assert_eq!(kind, io::ErrorKind::WouldBlock),
            Some(SendStop::Error(error)) => assert_eq!(error.kind(), kind),
            None => panic!("missing send stop"),
        }
        assert_eq!(calls, 2);
    }
    let report = submit_prefix(&[], BATCH_SIZE, |_| panic!("empty send submitted"));
    assert_eq!(report.accepted, 0);
    assert!(report.stop.is_none());
    let report = submit_prefix(&entries, 1, |group| Ok(group.len()));
    assert_eq!(report.accepted, entries.len());
    assert!(report.stop.is_none());
    let large = [entry(b"a", None); BATCH_SIZE + 1];
    let report = submit_prefix(&large, BATCH_SIZE, |group| Ok(group.len()));
    assert_eq!(report.accepted, BATCH_SIZE);
    assert!(matches!(report.stop, Some(SendStop::Blocked)));
}

#[test]
fn gso_validation_is_whole_input_and_never_implicitly_falls_back() {
    let caps = Capabilities {
        max_batch_messages: BATCH_SIZE,
        gro: true,
        max_gso_segments: MAX_GSO_SEGMENTS,
    };
    assert!(validate_send(&[entry(b"abcde", Some(2))], caps).is_ok());
    for (payload, segment) in [(vec![0; 65], 1), (vec![0; 2], 2), (vec![0; 65_536], 1200)] {
        assert!(
            validate_send(
                &[entry(b"valid", None), entry(&payload, Some(segment))],
                caps
            )
            .is_err()
        );
    }
    let scalar = Capabilities {
        max_gso_segments: 1,
        gro: false,
        max_batch_messages: 1,
    };
    assert_eq!(
        validate_send(&[entry(b"abc", Some(2))], scalar)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(validate_send(&[entry(&[], None)], scalar).is_ok());
    assert!(validate_send(&[entry(&vec![0; MAX_DATAGRAM_BYTES + 1], None)], caps).is_err());
}

#[test]
fn borrowed_receive_splits_short_gro_tail_and_zero_length_datagrams() {
    let mut arena = ReceiveArena::new(2048, true);
    arena
        .bytes
        .get_mut(..7)
        .unwrap()
        .copy_from_slice(b"abcdefg");
    let source = "127.0.0.1:1".parse().unwrap();
    let destination = "127.0.0.1:2".parse().unwrap();
    let received_at = ReceiveTime::now();
    arena.entries[0] = Some(Received {
        source,
        destination,
        received_at,
        len: 7,
        stride: 3,
    });
    arena.entries[1] = Some(Received {
        source,
        destination,
        received_at,
        len: 0,
        stride: 1,
    });
    let batch = RecvBatch { arena: &arena };
    let datagrams: Vec<_> = batch.iter().collect();
    assert_eq!(
        datagrams.iter().map(|d| d.payload).collect::<Vec<_>>(),
        [b"abc".as_slice(), b"def", b"g", b""]
    );
    for datagram in datagrams {
        assert_eq!(datagram.source, source);
        assert_eq!(datagram.destination, destination);
        assert_eq!(datagram.received_at, received_at);
        let pointer = datagram.payload.as_ptr() as usize;
        assert!(pointer >= arena.bytes.as_ptr() as usize);
        assert!(pointer < (arena.bytes.as_ptr() as usize).saturating_add(arena.bytes.len()));
    }
}

#[cfg(not(feature = "sim"))]
#[test]
fn production_socket_can_move_with_its_single_owner_between_workers() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<BoundUdpSocket>();
    send_sync::<UdpSocket>();
}

#[cfg(not(feature = "sim"))]
#[tokio::test]
async fn native_modes_receive_limits_mixed_destinations_and_reuse() {
    for mode in [Mode::Auto, Mode::Scalar] {
        let mut options = BindOptions::new("127.0.0.1:0".parse().unwrap());
        options.mode = mode;
        options.max_datagram_bytes = 4096;
        let mut socket = bind(options.clone()).await.unwrap().activate().unwrap();
        let bound = socket.local_addr();
        let first = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let second = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let data = vec![42; 2048];
        let destinations = [first.local_addr().unwrap(), second.local_addr().unwrap()];
        for _ in 0..8 {
            socket.writable().await.unwrap();
            let send = [
                SendDatagram {
                    destination: destinations[0],
                    payload: &data,
                    gso_segment_size: None,
                },
                SendDatagram {
                    destination: destinations[1],
                    payload: b"short",
                    gso_segment_size: None,
                },
            ];
            let report = socket.try_send_batch(&send);
            assert_eq!(report.accepted, 2);
            assert!(report.stop.is_none());
            let mut bytes = vec![0; 4097];
            assert_eq!(first.recv_from(&mut bytes).await.unwrap().0, data.len());
            assert_eq!(&bytes[..data.len()], &data);
            assert_eq!(second.recv_from(&mut bytes).await.unwrap().0, 5);
            first.send_to(&data, socket.local_addr()).await.unwrap();
            socket.readable().await.unwrap();
            let batch = socket.try_recv_batch().unwrap();
            let received: Vec<_> = batch.iter().collect();
            assert_eq!(received.len(), 1);
            assert_eq!(received[0].payload, &data);
            assert_eq!(received[0].source, destinations[0]);
            assert_eq!(received[0].destination, bound);
        }
        first
            .send_to(&vec![0; 4097], socket.local_addr())
            .await
            .unwrap();
        socket.readable().await.unwrap();
        assert_eq!(socket.try_recv_batch().unwrap().iter().count(), 0);
        let counters = socket.counters();
        assert_eq!(counters.received_datagrams, 8);
        assert_eq!(counters.submitted_datagrams, 16);
        assert_eq!(
            counters.truncated_drops + counters.invalid_metadata_drops,
            1
        );
        first.send_to(&[], socket.local_addr()).await.unwrap();
        socket.readable().await.unwrap();
        assert_eq!(
            socket
                .try_recv_batch()
                .unwrap()
                .iter()
                .next()
                .unwrap()
                .payload,
            &[]
        );
    }
}

#[cfg(all(target_os = "linux", not(feature = "sim")))]
#[tokio::test]
async fn dual_stack_reader_preserves_actual_ipv4_and_ipv6_destinations() {
    for mode in [Mode::Batch, Mode::Scalar] {
        let mut options = BindOptions::new("[::]:0".parse().unwrap());
        options.mode = mode;
        let mut socket = bind(options)
            .await
            .expect("Linux transport host requires dual-stack loopback")
            .activate()
            .unwrap();
        let port = socket.local_addr().port();
        let v4 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let v6 = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
        // Alternate families across reused payload storage and kernel metadata.
        for _ in 0..8 {
            for (sender, destination) in [
                (&v4, SocketAddr::from(([127, 0, 0, 1], port))),
                (&v6, SocketAddr::new("::1".parse().unwrap(), port)),
            ] {
                sender.send_to(b"destination", destination).await.unwrap();
                socket.readable().await.unwrap();
                let batch = socket.try_recv_batch().unwrap();
                let datagram = batch.iter().next().unwrap();
                assert_eq!(datagram.source, sender.local_addr().unwrap());
                assert_eq!(datagram.destination, destination);
            }
            let outbound = [
                SendDatagram {
                    destination: v4.local_addr().unwrap(),
                    payload: b"v4",
                    gso_segment_size: None,
                },
                SendDatagram {
                    destination: v6.local_addr().unwrap(),
                    payload: b"v6",
                    gso_segment_size: None,
                },
            ];
            socket.writable().await.unwrap();
            let report = socket.try_send_batch(&outbound);
            assert_eq!(report.accepted, 2, "{report:?}");
            assert!(report.stop.is_none());
            let mut bytes = [0; 8];
            for (receiver, payload) in [(&v4, b"v4"), (&v6, b"v6")] {
                let (len, source) = receiver.recv_from(&mut bytes).await.unwrap();
                assert_eq!(&bytes[..len], payload);
                assert_eq!(source.port(), port);
                assert_eq!(source.is_ipv4(), receiver.local_addr().unwrap().is_ipv4());
            }
        }
    }
}

#[cfg(all(target_os = "linux", not(feature = "sim")))]
#[tokio::test]
async fn gso_gro_preserves_a_full_segment_batch() {
    let mut receiver = bind(BindOptions::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap()
        .activate()
        .unwrap();
    let mut sender = bind(BindOptions::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap()
        .activate()
        .unwrap();
    assert!(
        receiver.capabilities().gro,
        "Linux transport host requires UDP GRO"
    );
    assert_eq!(
        sender.capabilities().max_gso_segments,
        MAX_GSO_SEGMENTS,
        "Linux transport host requires UDP GSO"
    );
    let segment_size = 800;
    let aggregate = (0..MAX_GSO_SEGMENTS)
        .flat_map(|index| {
            vec![
                u8::try_from(index).unwrap();
                if index == MAX_GSO_SEGMENTS - 1 {
                    237
                } else {
                    segment_size
                }
            ]
        })
        .collect::<Vec<_>>();
    // Larger than the previous GSO segment: a stale cmsg would split this.
    let ordinary = vec![7; 1200];
    for _ in 0..8 {
        sender.writable().await.unwrap();
        let entries = [
            SendDatagram {
                destination: receiver.local_addr(),
                payload: &aggregate,
                gso_segment_size: NonZeroU16::new(u16::try_from(segment_size).unwrap()),
            },
            SendDatagram {
                destination: receiver.local_addr(),
                payload: &ordinary,
                gso_segment_size: None,
            },
        ];
        let report = sender.try_send_batch(&entries);
        assert_eq!(report.accepted, 2, "{report:?}");
        assert!(report.stop.is_none());
        let mut received = Vec::new();
        while received.len() < MAX_GSO_SEGMENTS + 1 {
            receiver.readable().await.unwrap();
            let batch = receiver.try_recv_batch().unwrap();
            received.extend(batch.iter().map(|datagram| datagram.payload.to_vec()));
        }
        for (index, payload) in received.iter().take(MAX_GSO_SEGMENTS).enumerate() {
            assert_eq!(
                payload.as_slice(),
                vec![
                    u8::try_from(index).unwrap();
                    if index == MAX_GSO_SEGMENTS - 1 {
                        237
                    } else {
                        segment_size
                    }
                ]
            );
        }
        assert_eq!(received.last().unwrap(), &ordinary);
    }
}
