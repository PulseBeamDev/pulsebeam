use super::*;
use simulation::{Io, Metadata, Readiness};
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

struct Device {
    received: VecDeque<(Vec<u8>, Metadata)>,
    accepted: Rc<RefCell<Vec<Vec<u8>>>>,
    capacity: usize,
    fatal_at: Option<usize>,
    caps: Capabilities,
}
impl Io for Device {
    fn local_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 1))
    }
    fn capabilities(&self) -> Capabilities {
        self.caps
    }
    fn readable(&self) -> Readiness<'_> {
        Box::pin(async { Ok(()) })
    }
    fn writable(&self) -> Readiness<'_> {
        Box::pin(async { Ok(()) })
    }
    fn receive(&mut self, payload: &mut [u8]) -> io::Result<Metadata> {
        let (bytes, meta) = self
            .received
            .pop_front()
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;
        if let Some(target) = payload.get_mut(..bytes.len()) {
            target.copy_from_slice(&bytes);
        }
        Ok(meta)
    }
    fn submit(&mut self, entry: &SendDatagram<'_>) -> io::Result<()> {
        let mut accepted = self.accepted.borrow_mut();
        if self.fatal_at == Some(accepted.len()) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        if accepted.len() == self.capacity {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        accepted.push(entry.payload.to_vec());
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn simulated_public_boundary_preserves_atomic_progress_metadata_and_counters() {
    for mode in [Mode::Scalar, Mode::Batch] {
        for fatal in [false, true] {
            let accepted = Rc::new(RefCell::new(Vec::new()));
            let mut options = BindOptions::new("127.0.0.1:1".parse().unwrap());
            options.mode = mode;
            options.max_datagram_bytes = 4;
            let time = ReceiveTime::now();
            let source = "[::1]:2".parse().unwrap();
            let destination = "[::1]:1".parse().unwrap();
            let metadata = |len, stride| Metadata {
                source,
                destination,
                len,
                stride,
                received_at: time,
            };
            let mut device = Device {
                received: VecDeque::new(),
                accepted: accepted.clone(),
                capacity: 2,
                fatal_at: fatal.then_some(2),
                caps: Capabilities {
                    max_batch_messages: if mode == Mode::Batch { BATCH_SIZE } else { 1 },
                    gro: mode == Mode::Batch,
                    max_gso_segments: if mode == Mode::Batch {
                        MAX_GSO_SEGMENTS
                    } else {
                        1
                    },
                },
            };
            device.received.push_back((b"abc".to_vec(), metadata(3, 3)));
            device.received.push_back((
                b"bad".to_vec(),
                Metadata {
                    destination: "0.0.0.0:1".parse().unwrap(),
                    ..metadata(3, 3)
                },
            ));
            device.received.push_back((vec![], metadata(100_000, 1)));
            if mode == Mode::Batch {
                device
                    .received
                    .push_back((b"abcdefg".to_vec(), metadata(7, 3)));
            }
            let mut socket = simulation::from_io(options, Box::new(device))
                .unwrap()
                .activate()
                .unwrap();
            let mut data = [SendDatagram {
                destination,
                payload: b"a",
                gso_segment_size: None,
            }; 3];
            let invalid = [
                data[0],
                SendDatagram {
                    destination,
                    payload: b"abc",
                    gso_segment_size: NonZeroU16::new(3),
                },
            ];
            let report = socket.try_send_batch(&invalid);
            assert_eq!(report.accepted, 0);
            assert!(matches!(report.stop, Some(SendStop::Error(_))));
            assert!(accepted.borrow().is_empty());
            if mode == Mode::Batch {
                data[0].payload = b"abc";
                data[0].gso_segment_size = NonZeroU16::new(2);
            }
            let report = socket.try_send_batch(&data);
            assert_eq!(report.accepted, 2);
            assert_eq!(accepted.borrow().len(), 2);
            assert_eq!(accepted.borrow()[0], data[0].payload);
            assert_eq!(
                socket.counters().submitted_datagrams,
                if mode == Mode::Batch { 3 } else { 2 }
            );
            assert_eq!(
                socket.counters().submitted_bytes,
                if mode == Mode::Batch { 4 } else { 2 }
            );
            if fatal {
                assert!(matches!(report.stop, Some(SendStop::Error(_))));
            } else {
                assert!(matches!(report.stop, Some(SendStop::Blocked)));
            }
            // Admission is not delivery: this device need not deliver anything
            // it accepted, and counters must not claim to measure network loss.
            tokio::time::advance(std::time::Duration::from_secs(5)).await;
            let mut payloads = Vec::new();
            loop {
                match socket.try_recv_batch() {
                    Ok(batch) => {
                        for item in batch.iter() {
                            assert_eq!(item.received_at, time);
                            assert_eq!(item.destination, destination);
                            assert_eq!(item.source, source);
                            payloads.push(item.payload.to_vec());
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("{error}"),
                }
            }
            assert_eq!(payloads[0], b"abc");
            if mode == Mode::Batch {
                assert_eq!(
                    &payloads[1..],
                    [b"abc".to_vec(), b"def".to_vec(), b"g".to_vec()]
                );
            } else {
                assert_eq!(payloads.len(), 1);
            }
            let counters = socket.counters();
            assert_eq!(counters.invalid_metadata_drops, 1);
            assert_eq!(counters.truncated_drops, 1);
            assert_eq!(
                socket.try_recv_batch().err().unwrap().kind(),
                io::ErrorKind::WouldBlock
            );
        }
    }
}
