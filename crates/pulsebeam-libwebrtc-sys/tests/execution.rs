use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use pulsebeam_webrtc_sys::{
    BuildEnvironmentError, ControlledPeerDriver, ControlledWorld, Environment, MAX_CONTROLLED_TIME,
    ManualClock, NetworkThread, RandomnessLease, RandomnessLeaseError, TaskQueue, TaskQueueFactory,
    WorkerThread,
};

static GLOBAL_HOOKS: Mutex<()> = Mutex::new(());

#[test]
fn cooperative_environment_runs_ready_work_in_deadline_order() {
    let clock = ManualClock::new(Duration::from_secs(10)).unwrap();
    let factory = TaskQueueFactory::cooperative(&clock).unwrap();
    let environment = Environment::builder()
        .task_queue_factory(&factory)
        .build()
        .unwrap();
    let first = factory.create_queue("first", Default::default()).unwrap();
    let second = factory.create_queue("second", Default::default()).unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));

    for (queue, delay, value) in [
        (&first, Duration::from_millis(5), 3),
        (&second, Duration::ZERO, 1),
        (&first, Duration::ZERO, 2),
        (&second, Duration::from_millis(2), 4),
    ] {
        let observed = observed.clone();
        queue
            .post_delayed(delay, move || observed.lock().unwrap().push(value))
            .unwrap();
    }

    assert_eq!(environment.now(), Duration::from_secs(10));
    assert_eq!(environment.clone().now(), Duration::from_secs(10));
    assert_eq!(factory.run_ready(), 2);
    assert_eq!(*observed.lock().unwrap(), [1, 2]);
    assert_eq!(factory.next_deadline(), Some(Duration::from_millis(10_002)));

    clock.advance(Duration::from_millis(2)).unwrap();
    assert_eq!(factory.run_ready(), 1);
    clock.advance(Duration::from_millis(3)).unwrap();
    assert_eq!(factory.run_ready(), 1);
    assert_eq!(*observed.lock().unwrap(), [1, 2, 4, 3]);
    assert_eq!(factory.next_deadline(), None);
}

#[test]
fn seeded_randomness_is_repeatable_exclusive_and_retained_by_roots() {
    let _exclusive = GLOBAL_HOOKS.lock().unwrap();
    fn sequence(seed: u64) -> Vec<u64> {
        let lease = RandomnessLease::acquire(seed).unwrap();
        let clock = ManualClock::new(Duration::ZERO).unwrap();
        let environment = Environment::builder()
            .clock(&clock)
            .randomness(&lease)
            .build()
            .unwrap();
        let values = (0..4).map(|_| lease.next_u64()).collect();
        assert_eq!(
            std::thread::spawn(move || RandomnessLease::acquire(seed).err())
                .join()
                .unwrap(),
            Some(RandomnessLeaseError::AlreadyAcquired)
        );
        drop(lease);
        assert_eq!(
            RandomnessLease::acquire(seed).err(),
            Some(RandomnessLeaseError::AlreadyAcquired)
        );
        drop(environment);
        values
    }

    let first = sequence(42);
    let second = sequence(42);
    assert_eq!(first, second);
    assert_ne!(first, sequence(43));
}

#[test]
fn early_queue_and_thread_drop_cancel_pending_work() {
    let clock = ManualClock::new(Duration::ZERO).unwrap();
    let factory = TaskQueueFactory::cooperative(&clock).unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    {
        let mut queue = factory.create_queue("dropped", Default::default()).unwrap();
        let ran = ran.clone();
        queue
            .post_delayed(Duration::from_secs(1), move || {
                ran.store(true, Ordering::Release)
            })
            .unwrap();
        queue.close();
        queue.close();
        assert!(!queue.post(|| panic!("closed queue ran work")));
    }
    clock.advance(Duration::from_secs(1)).unwrap();
    assert_eq!(factory.run_ready(), 0);
    assert!(!ran.load(Ordering::Acquire));

    let ran = Arc::new(AtomicBool::new(false));
    {
        let mut thread = WorkerThread::start().unwrap();
        let ran = ran.clone();
        thread
            .post_delayed(Duration::from_secs(60), move || {
                ran.store(true, Ordering::Release)
            })
            .unwrap();
        thread.close();
        thread.close();
        assert!(!thread.post(|| panic!("closed thread ran work")));
    }
    assert!(!ran.load(Ordering::Acquire));

    // Socket-server construction and idempotent Rust cleanup are covered too.
    drop(NetworkThread::start().unwrap());
    drop(pulsebeam_webrtc_sys::SignalingThread::start().unwrap());
}

#[test]
fn mismatched_clock_fails_without_consuming_dependencies() {
    let first = ManualClock::new(Duration::ZERO).unwrap();
    let second = ManualClock::new(Duration::ZERO).unwrap();
    let factory = TaskQueueFactory::cooperative(&first).unwrap();
    assert!(
        Environment::builder()
            .clock(&second)
            .task_queue_factory(&factory)
            .build()
            .is_err()
    );

    let queue = factory
        .create_queue("still-live", Default::default())
        .unwrap();
    assert!(queue.post(|| {}));
    assert_eq!(factory.run_ready(), 1);
}

#[test]
fn cooperative_factory_rejects_wrong_thread_and_default_queues_execute() {
    let clock = ManualClock::new(Duration::ZERO).unwrap();
    let factory = TaskQueueFactory::cooperative(&clock).unwrap();
    let queue = factory
        .create_queue("serialized", Default::default())
        .unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let observed = ran.clone();
    assert!(queue.post(move || observed.store(true, Ordering::Release)));
    let other_factory = factory.clone();
    std::thread::spawn(move || {
        assert_eq!(other_factory.run_ready(), 0);
        assert_eq!(
            other_factory.pump(1),
            Err(BuildEnvironmentError::WrongThread)
        );
        assert!(matches!(
            other_factory.create_queue("wrong-thread", Default::default()),
            Err(BuildEnvironmentError::WrongThread)
        ));
    })
    .join()
    .unwrap();
    assert!(!ran.load(Ordering::Acquire));
    assert_eq!(factory.pump(1).unwrap().dispatched, 1);
    assert!(ran.load(Ordering::Acquire));

    let default_factory = TaskQueueFactory::default_threaded().unwrap();
    let default_queue = default_factory
        .create_queue("default", Default::default())
        .unwrap();
    let (completed_tx, completed_rx) = mpsc::channel();
    assert!(default_queue.post(move || completed_tx.send(()).unwrap()));
    completed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        default_queue.post_local(|| {}),
        Err(BuildEnvironmentError::NotCooperative)
    ));
}

#[test]
fn shared_clock_rejects_foreign_queue_creators_without_stalling_driver() {
    let _exclusive = GLOBAL_HOOKS.lock().unwrap();
    let clock = ManualClock::new(Duration::ZERO).unwrap();
    let factory = TaskQueueFactory::cooperative(&clock).unwrap();
    let driver = ControlledPeerDriver::new(&clock).unwrap();
    let queue = factory.create_queue("caller", Default::default()).unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let observed = ran.clone();
    assert!(queue.post(move || observed.store(true, Ordering::Release)));
    let other_clock = clock.clone();
    std::thread::spawn(move || {
        assert!(matches!(
            TaskQueueFactory::cooperative(&other_clock),
            Err(BuildEnvironmentError::NativeConstructionFailed)
        ));
    })
    .join()
    .unwrap();
    assert_eq!(driver.pump(1).dispatched, 1);
    assert!(ran.load(Ordering::Acquire));
    assert_eq!(driver.next_deadline(), None);
    drop(queue);
    drop(factory);
    drop(driver);

    // A pre-existing foreign factory must also reject driver acquisition,
    // without taking global hooks. Once it is gone, acquisition works again.
    let other_clock = clock.clone();
    let foreign = std::thread::spawn(move || TaskQueueFactory::cooperative(&other_clock).unwrap())
        .join()
        .unwrap();
    assert!(ControlledPeerDriver::new(&clock).is_err());
    drop(foreign);
    let driver = ControlledPeerDriver::new(&clock).unwrap();
    assert_eq!(driver.pump(1).dispatched, 0);
}

#[test]
fn bounded_world_pump_returns_under_self_reposting_and_retains_hooks() {
    let _exclusive = GLOBAL_HOOKS.lock().unwrap();
    fn repost(queue: Rc<RefCell<TaskQueue>>, count: Rc<Cell<usize>>) {
        let next_queue = queue.clone();
        queue
            .borrow()
            .post_local(move || {
                count.set(count.get() + 1);
                repost(next_queue, count);
            })
            .unwrap();
    }
    for _ in 0..2 {
        let world = ControlledWorld::acquire(42, Duration::from_secs(10)).unwrap();
        assert!(ControlledWorld::acquire(43, Duration::ZERO).is_err());
        assert_eq!(
            RandomnessLease::acquire(43).err(),
            Some(RandomnessLeaseError::AlreadyAcquired)
        );
        let queue = Rc::new(RefCell::new(
            world.create_queue("reposting", Default::default()).unwrap(),
        ));
        let count = Rc::new(Cell::new(0));
        repost(queue.clone(), count.clone());
        assert_eq!(world.pump(0).dispatched, 0);
        assert_eq!(count.get(), 0);
        let result = world.pump(7);
        assert_eq!(result.dispatched, 7);
        assert!(result.ready);
        assert_eq!(result.next_deadline, Some(Duration::from_secs(10)));
        assert_eq!(count.get(), 7);
        assert_eq!(world.now(), Duration::from_secs(10));
        assert_eq!(world.pump(3).dispatched, 3);
        assert_eq!(count.get(), 10);
        assert!(world.driver().run_ready());
        assert_eq!(count.get(), 1_034);
        assert!(world.pump(0).ready);
        assert_eq!(world.now(), Duration::from_secs(10));
        drop(world);
        assert!(ControlledWorld::acquire(42, Duration::ZERO).is_err());
        queue.borrow_mut().close();
        drop(queue);
        let next = ControlledWorld::acquire(42, Duration::ZERO).unwrap();
        assert_eq!(next.next_deadline(), None);
    }
    {
        let clock = ManualClock::new(Duration::ZERO).unwrap();
        let factory = TaskQueueFactory::cooperative(&clock).unwrap();
        let queue = Rc::new(RefCell::new(
            factory
                .create_queue("legacy-reposting", Default::default())
                .unwrap(),
        ));
        let count = Rc::new(Cell::new(0));
        repost(queue.clone(), count.clone());
        assert_eq!(factory.run_ready(), 1_024);
        assert_eq!(count.get(), 1_024);
        assert!(factory.pump(0).unwrap().ready);
        assert_eq!(clock.now(), Duration::ZERO);
        queue.borrow_mut().close();
    }
    let randomness = RandomnessLease::acquire(9).unwrap();
    let first = randomness.next_u64();
    assert!(ControlledWorld::acquire(42, Duration::ZERO).is_err());
    let second = randomness.next_u64();
    drop(randomness);
    let randomness = RandomnessLease::acquire(9).unwrap();
    assert_eq!(first, randomness.next_u64());
    assert_eq!(second, randomness.next_u64());
}

#[test]
fn clock_range_and_delayed_work_reject_before_mutation() {
    assert!(matches!(
        ManualClock::new(MAX_CONTROLLED_TIME + Duration::from_micros(1)),
        Err(BuildEnvironmentError::TimestampOutOfRange)
    ));
    let clock = ManualClock::new(MAX_CONTROLLED_TIME).unwrap();
    assert_eq!(
        clock.advance(Duration::from_micros(1)),
        Err(BuildEnvironmentError::TimestampOutOfRange)
    );
    assert_eq!(clock.now(), MAX_CONTROLLED_TIME);
    let factory = TaskQueueFactory::cooperative(&clock).unwrap();
    let queue = factory.create_queue("range", Default::default()).unwrap();
    assert_eq!(
        queue.post_delayed(Duration::from_micros(1), || {}),
        Err(BuildEnvironmentError::TimestampOutOfRange)
    );
    assert_eq!(factory.next_deadline(), None);
    assert!(queue.post(|| {}));
    assert_eq!(factory.pump(1).unwrap().dispatched, 1);
}
