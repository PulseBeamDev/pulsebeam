#![allow(clippy::unwrap_used, clippy::panic)]

use super::super::ManagedIo;
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Probe {
    inbox: mpsc::Receiver<u32>,
    observed: mpsc::UnboundedSender<(usize, u32)>,
    dropped: Arc<AtomicUsize>,
    deadline: Instant,
    timer_fired: bool,
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl Service for Probe {
    fn poll_service(&mut self, io: &mut ManagedIo, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let address = std::ptr::from_ref(self).addr();
        if let Poll::Ready(Some(value)) = self.inbox.poll_recv(cx) {
            let _ = self.observed.send((address, value));
            cx.waker().wake_by_ref();
        }
        if !self.timer_fired && io.poll_deadline(cx, Some(self.deadline)).is_ready() {
            self.timer_fired = true;
            let _ = self.observed.send((address, 99));
        }
        Poll::Pending
    }
}

#[tokio::test(start_paused = true)]
async fn same_pinned_service_moves_with_pending_queue_timer_and_owned_cleanup() {
    let shutdown = CancellationToken::new();
    let (events, mut completed) = mpsc::channel(64);
    let (source, source_rx) = mpsc::channel(8);
    let (destination, mut destination_rx) = mpsc::channel(8);
    let source_task = tokio::spawn(worker_loop(0, source_rx, events.clone(), shutdown.clone()));
    let source_slots = Arc::new(Semaphore::new(2));
    let dest_slots = Arc::new(Semaphore::new(2));
    let dropped = Arc::new(AtomicUsize::new(0));
    let (send, inbox) = mpsc::channel(8);
    let (observed, mut reports) = mpsc::unbounded_channel();
    let socket = pulsebeam_udp::bind(pulsebeam_udp::BindOptions::new(
        "127.0.0.1:0".parse().unwrap(),
    ))
    .await
    .unwrap();
    let drop_counter = dropped.clone();
    let deadline = Instant::now() + Duration::from_secs(1);
    let (ready, started) = oneshot::channel();
    source
        .send(Command::Start {
            id: 7,
            slot: source_slots.clone().try_acquire_owned().unwrap(),
            ready,
            factory: Box::new(move || {
                Ok(Box::pin(ManagedService::new(
                    Probe {
                        inbox,
                        observed,
                        dropped: drop_counter,
                        deadline,
                        timer_fired: false,
                    },
                    socket.activate()?,
                )))
            }),
        })
        .await
        .unwrap();
    started.await.unwrap().unwrap();
    send.send(1).await.unwrap();
    let (address, value) = reports.recv().await.unwrap();
    assert_eq!(value, 1);
    let (blocked, blocked_receiver) = mpsc::channel(1);
    blocked.try_send(Command::Snapshot(0)).unwrap();
    let commands = [source.clone(), blocked.clone()];
    let slots = [source_slots.clone(), dest_slots.clone()];
    assert!(request_move(7, 0, 1, 1, &commands, &slots).is_err());
    drop(blocked_receiver);
    assert!(request_move(7, 0, 1, 1, &commands, &slots).is_err());
    send.send(8).await.unwrap();
    assert_eq!(reports.recv().await.unwrap(), (address, 8));
    assert_eq!(dest_slots.available_permits(), 2);
    // Request an idle source, then leave input queued while ownership is in transit.
    let delivery = destination.clone().try_reserve_owned().unwrap();
    let rollback = source.clone().try_reserve_owned().unwrap();
    source
        .send(Command::Move {
            id: 7,
            request: MoveRequest {
                generation: 1,
                slot: dest_slots.clone().try_acquire_owned().unwrap(),
                delivery,
                rollback,
            },
        })
        .await
        .unwrap();
    source
        .send(Command::Move {
            id: 7,
            request: MoveRequest {
                generation: 1,
                slot: dest_slots.clone().try_acquire_owned().unwrap(),
                delivery: destination.clone().try_reserve_owned().unwrap(),
                rollback: source.clone().try_reserve_owned().unwrap(),
            },
        })
        .await
        .unwrap();
    send.send(2).await.unwrap();
    let handoff = destination_rx.recv().await.unwrap();
    assert!(matches!(handoff, Command::Arrive(_)));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(reports.try_recv().is_err());
    destination.send(handoff).await.unwrap();
    let destination_task = tokio::spawn(worker_loop(1, destination_rx, events, shutdown.clone()));
    match completed.recv().await.unwrap() {
        Event::Moved {
            id,
            source,
            destination,
            generation,
        } => assert_eq!((id, source, destination, generation), (7, 0, 1, 1)),
        _ => panic!("missing completed migration"),
    }
    assert_eq!(reports.recv().await.unwrap(), (address, 2));
    assert_eq!(reports.recv().await.unwrap(), (address, 99));
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    assert_eq!(source_slots.available_permits(), 2);
    destination.send(Command::Snapshot(2)).await.unwrap();
    let Some(Event::Snapshot(snapshot)) = completed.recv().await else {
        panic!("missing post-handoff observation");
    };
    assert!(
        snapshot
            .services
            .first()
            .is_some_and(|service| service.id == 7
                && service.generation == 1
                && service.at == Instant::now())
    );
    shutdown.cancel();
    source_task.await.unwrap().unwrap();
    destination_task.await.unwrap().unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(dest_slots.available_permits(), 2);
}

#[tokio::test(start_paused = true)]
async fn closed_and_stale_requests_leave_exclusive_source_ownership() {
    let shutdown = CancellationToken::new();
    let (events, mut completed) = mpsc::channel(8);
    let (source, receiver) = mpsc::channel(8);
    let worker = tokio::spawn(worker_loop(0, receiver, events, shutdown.clone()));
    let slots = Arc::new(Semaphore::new(2));
    let (dest, dest_receiver) = mpsc::channel(1);
    let delivery = dest.clone().try_reserve_owned().unwrap();
    let rollback = source.clone().try_reserve_owned().unwrap();
    source
        .send(Command::Move {
            id: 100,
            request: MoveRequest {
                generation: 2,
                slot: slots.clone().try_acquire_owned().unwrap(),
                delivery,
                rollback,
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        completed.recv().await,
        Some(Event::Rejected { .. })
    ));
    drop(dest_receiver);
    assert!(dest.try_reserve_owned().is_err());
    assert_eq!(slots.available_permits(), 2);
    shutdown.cancel();
    worker.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn failed_handoffs_restore_the_source_or_fail_the_node_and_drop_once() {
    for (fail_detach, fail_activations, fatal) in
        [(true, 0, false), (false, 1, false), (false, 2, true)]
    {
        let shutdown = CancellationToken::new();
        let (events, mut completed) = mpsc::channel(8);
        let (source, source_rx) = mpsc::channel(8);
        let (destination, destination_rx) = mpsc::channel(8);
        let source_task = tokio::spawn(worker_loop(0, source_rx, events.clone(), shutdown.clone()));
        let dest_task = tokio::spawn(worker_loop(1, destination_rx, events, shutdown.clone()));
        let source_slots = Arc::new(Semaphore::new(2));
        let dest_slots = Arc::new(Semaphore::new(2));
        let dropped = Arc::new(AtomicUsize::new(0));
        let (send, inbox) = mpsc::channel(8);
        let (observed, mut reports) = mpsc::unbounded_channel();
        let socket = pulsebeam_udp::bind(pulsebeam_udp::BindOptions::new(
            "127.0.0.1:0".parse().unwrap(),
        ))
        .await
        .unwrap();
        let drop_counter = dropped.clone();
        let deadline = Instant::now() + Duration::from_secs(3600);
        let (ready, started) = oneshot::channel();
        source
            .send(Command::Start {
                id: 0,
                ready,
                slot: source_slots.clone().try_acquire_owned().unwrap(),
                factory: Box::new(move || {
                    Ok(Box::pin(
                        ManagedService::new(
                            Probe {
                                inbox,
                                observed,
                                dropped: drop_counter,
                                deadline,
                                timer_fired: false,
                            },
                            socket.activate()?,
                        )
                        .inject_failures(fail_detach, fail_activations),
                    ))
                }),
            })
            .await
            .unwrap();
        started.await.unwrap().unwrap();
        send.send(1).await.unwrap();
        let (address, _) = reports.recv().await.unwrap();
        source
            .send(Command::Move {
                id: 0,
                request: MoveRequest {
                    generation: 1,
                    slot: dest_slots.clone().try_acquire_owned().unwrap(),
                    delivery: destination.clone().try_reserve_owned().unwrap(),
                    rollback: source.clone().try_reserve_owned().unwrap(),
                },
            })
            .await
            .unwrap();
        if fatal {
            shutdown.cancelled().await;
        } else {
            assert!(matches!(
                completed.recv().await,
                Some(Event::Rejected { .. })
            ));
            assert!(!shutdown.is_cancelled());
            send.send(2).await.unwrap();
            assert_eq!(reports.recv().await.unwrap(), (address, 2));
            assert_eq!(dest_slots.available_permits(), 2);
        }
        shutdown.cancel();
        assert_eq!(source_task.await.unwrap().is_err(), fatal);
        dest_task.await.unwrap().unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(source_slots.available_permits(), 2);
        assert_eq!(dest_slots.available_permits(), 2);
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_drains_a_queued_handoff_without_polling_or_duplicate_drop() {
    let shutdown = CancellationToken::new();
    let (events, _completed) = mpsc::channel(8);
    let (source, source_rx) = mpsc::channel(8);
    let (destination, mut destination_rx) = mpsc::channel(8);
    let source_task = tokio::spawn(worker_loop(0, source_rx, events.clone(), shutdown.clone()));
    let slots = Arc::new(Semaphore::new(4));
    let dropped = Arc::new(AtomicUsize::new(0));
    let (send, inbox) = mpsc::channel(8);
    let (observed, mut reports) = mpsc::unbounded_channel();
    let socket = pulsebeam_udp::bind(pulsebeam_udp::BindOptions::new(
        "127.0.0.1:0".parse().unwrap(),
    ))
    .await
    .unwrap();
    let drop_counter = dropped.clone();
    let deadline = Instant::now() + Duration::from_secs(3600);
    let (ready, started) = oneshot::channel();
    source
        .send(Command::Start {
            id: 0,
            ready,
            slot: slots.clone().try_acquire_owned().unwrap(),
            factory: Box::new(move || {
                Ok(Box::pin(ManagedService::new(
                    Probe {
                        inbox,
                        observed,
                        dropped: drop_counter,
                        deadline,
                        timer_fired: false,
                    },
                    socket.activate()?,
                )))
            }),
        })
        .await
        .unwrap();
    started.await.unwrap().unwrap();
    send.send(1).await.unwrap();
    reports.recv().await.unwrap();
    source
        .send(Command::Move {
            id: 0,
            request: MoveRequest {
                generation: 1,
                slot: slots.clone().try_acquire_owned().unwrap(),
                delivery: destination.clone().try_reserve_owned().unwrap(),
                rollback: source.clone().try_reserve_owned().unwrap(),
            },
        })
        .await
        .unwrap();
    let arrival = destination_rx.recv().await.unwrap();
    assert!(matches!(arrival, Command::Arrive(_)));
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    destination.send(arrival).await.unwrap();
    shutdown.cancel();
    let dest_task = tokio::spawn(worker_loop(1, destination_rx, events, shutdown));
    source_task.await.unwrap().unwrap();
    dest_task.await.unwrap().unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(slots.available_permits(), 4);
    assert!(reports.recv().await.is_none());
}

struct Hot {
    id: usize,
    observed: mpsc::UnboundedSender<(usize, bool)>,
    first: bool,
    timer: bool,
    deadline: Instant,
}

impl Service for Hot {
    fn poll_service(&mut self, io: &mut ManagedIo, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if std::mem::take(&mut self.first) {
            let _ = self.observed.send((self.id, false));
        }
        if !self.timer && io.poll_deadline(cx, Some(self.deadline)).is_ready() {
            self.timer = true;
            let _ = self.observed.send((self.id, true));
        }
        io.yield_after_tick(cx);
        Poll::Pending
    }
}

#[tokio::test(start_paused = true)]
async fn continuously_ready_services_share_a_worker_and_keep_timers_and_control_live() {
    let shutdown = CancellationToken::new();
    let (events, mut snapshots) = mpsc::channel(8);
    let (commands, receiver) = mpsc::channel(8);
    let worker = tokio::spawn(worker_loop(0, receiver, events, shutdown.clone()));
    let (observed, mut reports) = mpsc::unbounded_channel();
    let slots = Arc::new(Semaphore::new(2));
    for id in 0..2 {
        let socket = pulsebeam_udp::bind(pulsebeam_udp::BindOptions::new(
            "127.0.0.1:0".parse().unwrap(),
        ))
        .await
        .unwrap();
        let observed = observed.clone();
        let deadline = Instant::now() + Duration::from_secs(1);
        let (ready, started) = oneshot::channel();
        commands
            .send(Command::Start {
                id,
                ready,
                slot: slots.clone().try_acquire_owned().unwrap(),
                factory: Box::new(move || {
                    Ok(Box::pin(ManagedService::new(
                        Hot {
                            id,
                            observed,
                            first: true,
                            timer: false,
                            deadline,
                        },
                        socket.activate()?,
                    )))
                }),
            })
            .await
            .unwrap();
        started.await.unwrap().unwrap();
    }
    let mut serviced = std::collections::HashSet::new();
    for _ in 0..2 {
        serviced.insert(reports.recv().await.unwrap());
    }
    assert_eq!(serviced, [(0, false), (1, false)].into_iter().collect());
    tokio::time::advance(Duration::from_secs(1)).await;
    serviced.clear();
    for _ in 0..2 {
        serviced.insert(reports.recv().await.unwrap());
    }
    assert_eq!(serviced, [(0, true), (1, true)].into_iter().collect());
    commands.send(Command::Snapshot(1)).await.unwrap();
    let Some(Event::Snapshot(snapshot)) = snapshots.recv().await else {
        panic!("missing complete pressure snapshot");
    };
    assert_eq!(snapshot.services.len(), 2);
    assert_eq!(snapshot.epoch, 1);
    assert!(snapshot.scheduling_wait > Duration::ZERO);
    for service in &snapshot.services {
        assert_eq!(service.generation, 0);
        assert!(service.at <= Instant::now());
        assert!(service.scheduling_wait > Duration::ZERO);
    }
    shutdown.cancel();
    worker.await.unwrap().unwrap();
    assert_eq!(slots.available_permits(), 2);
}

#[tokio::test(start_paused = true)]
async fn observations_do_not_move_services_without_an_explicit_request() {
    let shutdown = CancellationToken::new();
    let (source, mut source_rx) = mpsc::channel(8);
    let (destination, mut destination_rx) = mpsc::channel(8);
    let (events, receiver) = mpsc::channel(8);
    let (observations, observed) = watch::channel(None);
    let (requests, relocations) = mpsc::channel(1);
    let handle = MigrationHandle { requests, observed };
    let control = tokio::spawn(coordinate_execution(
        vec![source, destination],
        vec![Arc::new(Semaphore::new(4)), Arc::new(Semaphore::new(4))],
        receiver,
        relocations,
        observations,
        shutdown.clone(),
    ));
    let mut published = handle.observed.clone();
    for round in 1..=5 {
        let Some(Command::Snapshot(epoch)) = source_rx.recv().await else {
            panic!("missing source observation request");
        };
        assert!(
            matches!(destination_rx.recv().await, Some(Command::Snapshot(other)) if other == epoch)
        );
        let sample = |id, busy| ServiceSnapshot {
            id,
            generation: 0,
            at: Instant::now(),
            busy,
            scheduling_wait: Duration::ZERO,
        };
        events
            .send(Event::Snapshot(WorkerSnapshot {
                worker: 0,
                epoch,
                scheduling_wait: Duration::ZERO,
                services: vec![
                    sample(0, Duration::from_millis(100 * round)),
                    sample(1, Duration::from_millis(80 * round)),
                ],
            }))
            .await
            .unwrap();
        events
            .send(Event::Snapshot(WorkerSnapshot {
                worker: 1,
                epoch,
                scheduling_wait: Duration::ZERO,
                services: vec![sample(2, Duration::from_millis(10 * round))],
            }))
            .await
            .unwrap();
        published.changed().await.unwrap();
        assert!(
            handle
                .with_snapshot(|snapshot| snapshot.is_some_and(|snapshot| snapshot.epoch == epoch))
        );
        assert!(source_rx.try_recv().is_err());
        assert!(destination_rx.try_recv().is_err());
        if round < 5 {
            tokio::time::advance(SAMPLE_INTERVAL).await;
        }
    }
    let mut migration = handle.try_move_service(1, 1).unwrap();
    assert!(!migration.try_complete().unwrap());
    let Some(Command::Move { id, request }) = source_rx.recv().await else {
        panic!("missing explicit move");
    };
    assert_eq!(id, 1);
    assert!(handle.with_snapshot(|snapshot| snapshot.is_none()));
    published.borrow_and_update();
    events
        .send(Event::Moved {
            id,
            source: 0,
            destination: 1,
            generation: request.generation,
        })
        .await
        .unwrap();
    published.changed().await.unwrap();
    assert!(migration.try_complete().unwrap());
    assert!(handle.with_snapshot(|snapshot| snapshot.is_none()));
    drop(request);
    shutdown.cancel();
    control.await.unwrap().unwrap();
}

#[tokio::test]
async fn repeated_instances_and_failed_service_start_are_joined() {
    for profile in [
        Profile::Adaptive,
        Profile::WorkStealing,
        Profile::ThreadPerCore,
    ] {
        let healthy_shutdown = CancellationToken::new();
        let mut healthy = DataExecutor::new(
            ExecutionConfig::new(profile, 1, 1),
            healthy_shutdown.clone(),
        )
        .await
        .unwrap();
        let socket = pulsebeam_udp::bind(pulsebeam_udp::BindOptions::new(
            "127.0.0.1:0".parse().unwrap(),
        ))
        .await
        .unwrap();
        let (send, inbox) = mpsc::channel(8);
        let (observed, mut reports) = mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        let drop_counter = dropped.clone();
        healthy
            .spawn_on(0, 0, move || {
                Ok(ManagedService::new(
                    Probe {
                        inbox,
                        observed,
                        dropped: drop_counter,
                        deadline: Instant::now() + Duration::from_secs(3600),
                        timer_fired: false,
                    },
                    socket.activate()?,
                ))
            })
            .await
            .unwrap();
        send.send(1).await.unwrap();
        let (address, value) = reports.recv().await.unwrap();
        assert_eq!(value, 1);
        assert_eq!(
            healthy
                .spawn_on::<Probe>(0, 0, || panic!("duplicate launch must not run"))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            healthy
                .spawn_on::<Probe>(2, 1, || panic!("invalid worker launch must not run"))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        for _ in 0..2 {
            let shutdown = CancellationToken::new();
            let mut executor = DataExecutor::new(ExecutionConfig::new(profile, 2, 1), shutdown)
                .await
                .unwrap();
            let result = executor
                .spawn_on::<Probe>(0, 0, || Err(io::Error::other("failed service startup")))
                .await;
            assert!(result.is_err());
            executor.finish().await.unwrap();
            assert!(!healthy_shutdown.is_cancelled());
            send.send(2).await.unwrap();
            assert_eq!(reports.recv().await.unwrap(), (address, 2));
        }
        healthy.finish().await.unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}
