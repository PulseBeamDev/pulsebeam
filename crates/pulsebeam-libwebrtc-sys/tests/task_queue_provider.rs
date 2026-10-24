use pulsebeam_webrtc_sys::{
    BuildEnvironmentError, Environment, ManualClock, ProductionSession, ProductionSessionConfig,
    QueuePriority, TaskDelayPrecision, TaskQueueContext, TaskQueueFactory,
    TaskQueueFactoryProvider, TaskQueueIdentity, TaskQueueLocation, TaskQueuePostError,
    TaskQueueProvider, TaskQueueTask, TimeDelta,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
struct Queue {
    name: Vec<u8>,
    tasks: Mutex<Vec<TaskQueueTask>>,
    delays: Mutex<Vec<(TimeDelta, TaskDelayPrecision)>>,
    deleted: AtomicUsize,
    panic_post: AtomicBool,
    panic_name: AtomicBool,
    panic_delete: AtomicBool,
}
impl TaskQueueProvider for Queue {
    fn queue_name(&self) -> Vec<u8> {
        assert!(!self.panic_name.load(Ordering::SeqCst), "queue name panic");
        self.name.clone()
    }
    fn post_task(&self, task: TaskQueueTask, _: TaskQueueLocation) {
        assert!(!self.panic_post.load(Ordering::SeqCst), "queue post panic");
        self.tasks.lock().unwrap().push(task);
    }
    fn post_delayed_task(
        &self,
        task: TaskQueueTask,
        delay: TimeDelta,
        precision: TaskDelayPrecision,
        _: TaskQueueLocation,
    ) {
        self.delays.lock().unwrap().push((delay, precision));
        self.post_task(task, TaskQueueLocation);
    }
    fn delete(&self) {
        self.deleted.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.panic_delete.load(Ordering::SeqCst),
            "queue delete panic"
        );
        // Intentionally retain tickets to test revocation, not just scheduler
        // cooperation. Native callable captures must nevertheless be released.
    }
}
#[derive(Default)]
struct Factory {
    requests: Mutex<Vec<(Vec<u8>, QueuePriority)>>,
    queues: Mutex<Vec<Arc<Queue>>>,
    panic_create: AtomicBool,
}
impl TaskQueueFactoryProvider for Factory {
    fn create_task_queue(
        &self,
        name: &[u8],
        priority: QueuePriority,
    ) -> Box<dyn TaskQueueProvider> {
        assert!(
            !self.panic_create.load(Ordering::SeqCst),
            "queue creation panic"
        );
        self.requests
            .lock()
            .unwrap()
            .push((name.to_vec(), priority));
        let queue = Arc::new(Queue {
            name: name.to_vec(),
            ..Queue::default()
        });
        self.queues.lock().unwrap().push(queue.clone());
        Box::new(queue)
    }
}
impl Factory {
    fn queue(&self, index: usize) -> Arc<Queue> {
        self.queues.lock().unwrap()[index].clone()
    }
}
fn drain(queue: &Queue) -> Vec<TaskQueueTask> {
    std::mem::take(&mut *queue.tasks.lock().unwrap())
}

#[test]
fn custom_runtime_types_are_send_but_products_remain_sequence_bound() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<TaskQueueFactory>();
    assert_sync::<TaskQueueFactory>();
    assert_send::<TaskQueueTask>();
    assert_send::<TaskQueueContext>();
}

#[test]
fn exact_creation_names_all_priorities_and_legacy_validation() {
    let provider = Arc::new(Factory::default());
    let factory = TaskQueueFactory::from_provider(provider.clone()).unwrap();
    assert!(factory.is_provider_backed());
    assert!(!factory.is_cooperative());
    for priority in [
        QueuePriority::Normal,
        QueuePriority::High,
        QueuePriority::Video,
        QueuePriority::Audio,
        QueuePriority::Low,
    ] {
        for name in [&[][..], &b"q\0\xff"[..]] {
            let mut queue = factory.create_queue_bytes(name, priority).unwrap();
            assert_eq!(queue.name_bytes(), name);
            assert!(!queue.is_current());
            assert_eq!(
                provider.requests.lock().unwrap().last(),
                Some(&(name.to_vec(), priority))
            );
            queue.close();
            assert!(queue.identity().is_none());
            assert!(!queue.post(|| {}));
            assert!(queue.name_bytes().is_empty());
        }
    }
    assert!(matches!(
        factory.create_queue("q\0x", QueuePriority::Normal),
        Err(BuildEnvironmentError::InvalidQueueName)
    ));
    assert!(!factory.has_failed());
    assert!(
        provider
            .queues
            .lock()
            .unwrap()
            .iter()
            .all(|queue| queue.deleted.load(Ordering::SeqCst) == 1)
    );
}

#[test]
fn serial_fifo_execution_and_owned_current_context() {
    let provider = Arc::new(Factory::default());
    let factory = TaskQueueFactory::from_provider(provider.clone()).unwrap();
    let mut queue = factory
        .create_queue_bytes(b"fifo\0\xff", QueuePriority::Normal)
        .unwrap();
    let identity = queue.identity().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let contexts = Arc::new(Mutex::new(Vec::new()));
    for number in 0..3 {
        let seen = seen.clone();
        let contexts = contexts.clone();
        assert!(queue.post(move || {
            let current = TaskQueueContext::current().unwrap();
            assert_eq!(current.identity(), identity);
            assert!(current.is_current());
            assert_eq!(current.name_bytes(), b"fifo\0\xff");
            contexts.lock().unwrap().push(current);
            seen.lock().unwrap().push(number);
        }));
    }
    let source = provider.queue(0);
    let mut tasks = drain(&source);
    // Accepted out-of-order delivery is held, not executed out of order.
    assert!(
        std::thread::spawn(move || tasks.remove(2).execute())
            .join()
            .unwrap()
    );
    // The moved vector's remaining tickets were dropped by that thread, which
    // cancels the two predecessors and releases the accepted successor.
    assert_eq!(*seen.lock().unwrap(), vec![2]);
    let retained = contexts.lock().unwrap()[0].clone();
    assert!(!retained.is_current());
    queue.close();
    assert_eq!(retained.name_bytes(), b"fifo\0\xff");
    assert_eq!(retained.identity(), identity);
}

#[test]
fn current_context_posts_asynchronously_and_cannot_post_from_another_thread() {
    let provider = Arc::new(Factory::default());
    let factory = TaskQueueFactory::from_provider(provider.clone()).unwrap();
    let mut queue = factory
        .create_queue("current", QueuePriority::Normal)
        .unwrap();
    let retained = Arc::new(Mutex::new(None));
    let ran = Arc::new(AtomicBool::new(false));
    let snapshot = retained.clone();
    let completed = ran.clone();
    assert!(queue.post(move || {
        let current = TaskQueueContext::current().unwrap();
        let ran = completed.clone();
        current
            .post(move || ran.store(true, Ordering::SeqCst))
            .unwrap();
        assert!(!completed.load(Ordering::SeqCst));
        let off_thread = current.clone();
        assert_eq!(
            std::thread::spawn(move || off_thread.post(|| {}))
                .join()
                .unwrap(),
            Err(TaskQueuePostError::NotCurrent)
        );
        *snapshot.lock().unwrap() = Some(current);
    }));
    let source = provider.queue(0);
    assert!(drain(&source).pop().unwrap().execute());
    assert!(!ran.load(Ordering::SeqCst));
    assert!(drain(&source).pop().unwrap().execute());
    assert!(ran.load(Ordering::SeqCst));
    queue.close();
    let current = retained.lock().unwrap().clone().unwrap();
    assert_eq!(current.post(|| {}), Err(TaskQueuePostError::NotCurrent));
}

#[test]
fn current_context_also_posts_on_non_provider_cooperative_queues() {
    let clock = ManualClock::new(std::time::Duration::from_secs(1)).unwrap();
    let factory = TaskQueueFactory::cooperative(&clock).unwrap();
    let queue = factory
        .create_queue("cooperative", QueuePriority::Normal)
        .unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let completed = ran.clone();
    assert!(queue.post(move || {
        let current = TaskQueueContext::current().unwrap();
        current
            .post(move || completed.store(true, Ordering::SeqCst))
            .unwrap();
    }));
    assert_eq!(factory.pump(1).unwrap().dispatched, 1);
    assert!(!ran.load(Ordering::SeqCst));
    assert_eq!(factory.pump(1).unwrap().dispatched, 1);
    assert!(ran.load(Ordering::SeqCst));
}

#[test]
fn current_context_rejects_posting_during_capture_retirement() {
    struct RepostOnDrop(Arc<AtomicBool>);
    impl Drop for RepostOnDrop {
        fn drop(&mut self) {
            let current = TaskQueueContext::current().unwrap();
            self.0.store(
                current.post(|| {}) == Err(TaskQueuePostError::QueueUnavailable),
                Ordering::SeqCst,
            );
        }
    }
    let provider = Arc::new(Factory::default());
    let factory = TaskQueueFactory::from_provider(provider.clone()).unwrap();
    let mut queue = factory
        .create_queue("retiring", QueuePriority::Normal)
        .unwrap();
    let rejected = Arc::new(AtomicBool::new(false));
    let capture = RepostOnDrop(rejected.clone());
    assert!(queue.post(move || drop(capture)));
    queue.close();
    assert!(rejected.load(Ordering::SeqCst));
    assert!(!drain(&provider.queue(0)).pop().unwrap().execute());
}

#[test]
fn reordered_submissions_execute_in_fifo_order() {
    let provider = Arc::new(Factory::default());
    let factory = TaskQueueFactory::from_provider(provider.clone()).unwrap();
    let queue = factory.create_queue("fifo", QueuePriority::Normal).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    for number in 0..3 {
        let seen = seen.clone();
        assert!(queue.post(move || seen.lock().unwrap().push(number)));
    }
    let mut tasks = drain(&provider.queue(0));
    assert!(tasks.remove(2).execute());
    assert!(seen.lock().unwrap().is_empty());
    assert!(tasks.remove(1).execute());
    assert!(seen.lock().unwrap().is_empty());
    assert!(tasks.remove(0).execute());
    assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2]);
}

struct Probe {
    contexts: Arc<Mutex<Vec<Option<TaskQueueIdentity>>>>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.contexts
            .lock()
            .unwrap()
            .push(TaskQueueContext::current().map(|current| current.identity()));
    }
}
#[test]
fn cancellation_teardown_and_retained_ticket_revocation() {
    let provider = Arc::new(Factory::default());
    let factory = TaskQueueFactory::from_provider(provider.clone()).unwrap();
    let mut queue = factory
        .create_queue("cancel", QueuePriority::Normal)
        .unwrap();
    let identity = queue.identity().unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let ran = Arc::new(AtomicUsize::new(0));
    for _ in 0..2 {
        let probe = Probe {
            contexts: contexts.clone(),
        };
        let ran = ran.clone();
        assert!(queue.post(move || {
            ran.fetch_add(1, Ordering::SeqCst);
            drop(probe);
        }));
    }
    let source = provider.queue(0);
    let mut tasks = drain(&source);
    let canceled = tasks.remove(0);
    std::thread::spawn(move || drop(canceled)).join().unwrap();
    assert_eq!(*contexts.lock().unwrap(), vec![Some(identity)]);
    queue.close();
    assert_eq!(
        *contexts.lock().unwrap(),
        vec![Some(identity), Some(identity)]
    );
    assert_eq!(source.deleted.load(Ordering::SeqCst), 1);
    let retired = tasks.remove(0);
    assert!(
        !std::thread::spawn(move || retired.execute())
            .join()
            .unwrap()
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    queue.close();
    assert_eq!(source.deleted.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_delivery_does_not_overlap_execution_or_capture_destruction() {
    let provider = Arc::new(Factory::default());
    let factory = TaskQueueFactory::from_provider(provider.clone()).unwrap();
    let queue = factory
        .create_queue("concurrent", QueuePriority::Normal)
        .unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let first_active = active.clone();
    assert!(queue.post(move || {
        assert_eq!(first_active.fetch_add(1, Ordering::SeqCst), 0);
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        assert_eq!(first_active.fetch_sub(1, Ordering::SeqCst), 1);
    }));
    let second_active = active.clone();
    assert!(queue.post(move || {
        assert_eq!(second_active.fetch_add(1, Ordering::SeqCst), 0);
        assert_eq!(second_active.fetch_sub(1, Ordering::SeqCst), 1);
    }));
    let canceled_active = active.clone();
    struct SerialDrop(Arc<AtomicUsize>);
    impl Drop for SerialDrop {
        fn drop(&mut self) {
            assert_eq!(self.0.fetch_add(1, Ordering::SeqCst), 0);
            assert_eq!(self.0.fetch_sub(1, Ordering::SeqCst), 1);
        }
    }
    let probe = SerialDrop(canceled_active);
    assert!(queue.post(move || drop(probe)));
    let mut tasks = drain(&provider.queue(0));
    let first = tasks.remove(0);
    let second = tasks.remove(0);
    let canceled = tasks.remove(0);
    let running = std::thread::spawn(move || first.execute());
    entered_rx.recv().unwrap();
    assert!(std::thread::spawn(move || second.execute()).join().unwrap());
    std::thread::spawn(move || drop(canceled)).join().unwrap();
    assert_eq!(active.load(Ordering::SeqCst), 1);
    release_tx.send(()).unwrap();
    assert!(running.join().unwrap());
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[test]
fn complete_native_delay_states_and_precision_values() {
    let provider = Arc::new(Factory::default());
    let factory = TaskQueueFactory::from_provider(provider.clone()).unwrap();
    let mut queue = factory.create_queue("delays", QueuePriority::Low).unwrap();
    let source = provider.queue(0);
    for bits in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
        for precision in [TaskDelayPrecision::Low, TaskDelayPrecision::High] {
            let delay = TimeDelta::from_native_microsecond_bits(bits);
            assert!(
                queue
                    .post_delayed_native(delay, precision, TaskQueueLocation::current(), || {})
                    .unwrap()
            );
            assert_eq!(
                source.delays.lock().unwrap().last(),
                Some(&(delay, precision))
            );
        }
    }
    queue.close();
    for task in drain(&source) {
        assert!(!task.execute());
    }
    let default = TaskQueueFactory::default_threaded().unwrap();
    let queue = default
        .create_queue("builtin", QueuePriority::Normal)
        .unwrap();
    assert!(
        queue
            .post_delayed_native(
                TimeDelta::plus_infinity(),
                TaskDelayPrecision::High,
                TaskQueueLocation,
                || {}
            )
            .unwrap()
    );
}

#[test]
fn callback_failure_latches_and_teardown_still_runs() {
    let provider = Arc::new(Factory::default());
    let factory = TaskQueueFactory::from_provider(provider.clone()).unwrap();
    let mut queue = factory
        .create_queue("panic", QueuePriority::Normal)
        .unwrap();
    let source = provider.queue(0);
    source.panic_post.store(true, Ordering::SeqCst);
    assert!(!queue.post(|| {}));
    assert!(queue.has_failed());
    assert!(factory.has_failed());
    source.panic_delete.store(true, Ordering::SeqCst);
    queue.close();
    assert_eq!(source.deleted.load(Ordering::SeqCst), 1);
    provider.panic_create.store(true, Ordering::SeqCst);
    assert!(matches!(
        factory.create_queue("failed", QueuePriority::Normal),
        Err(BuildEnvironmentError::NativeConstructionFailed)
    ));
    assert!(matches!(
        factory.create_queue("disabled", QueuePriority::Normal),
        Err(BuildEnvironmentError::NativeConstructionFailed)
    ));
}

#[test]
fn name_failure_retires_pending_delivery_without_running_it() {
    let provider = Arc::new(Factory::default());
    let factory = TaskQueueFactory::from_provider(provider.clone()).unwrap();
    let queue = factory
        .create_queue("name panic", QueuePriority::Normal)
        .unwrap();
    let source = provider.queue(0);
    let ran = Arc::new(AtomicBool::new(false));
    let copy = ran.clone();
    assert!(queue.post(move || copy.store(true, Ordering::SeqCst)));
    source.panic_name.store(true, Ordering::SeqCst);
    assert_eq!(queue.name_bytes(), b"unnamed");
    assert!(queue.has_failed());
    assert!(!drain(&source).remove(0).execute());
    assert!(!ran.load(Ordering::SeqCst));
}

struct Inline;
impl TaskQueueFactoryProvider for Inline {
    fn create_task_queue(&self, _: &[u8], _: QueuePriority) -> Box<dyn TaskQueueProvider> {
        Box::new(Inline)
    }
}
impl TaskQueueProvider for Inline {
    fn post_task(&self, task: TaskQueueTask, _: TaskQueueLocation) {
        assert!(!task.execute());
    }
    fn post_delayed_task(
        &self,
        task: TaskQueueTask,
        _: TimeDelta,
        _: TaskDelayPrecision,
        _: TaskQueueLocation,
    ) {
        assert!(!task.execute());
    }
}
#[test]
fn inline_delivery_is_rejected_before_native_entry() {
    let factory = TaskQueueFactory::from_provider(Inline).unwrap();
    let queue = factory
        .create_queue("inline", QueuePriority::Normal)
        .unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let copy = ran.clone();
    assert!(!queue.post(move || copy.store(true, Ordering::SeqCst)));
    assert!(!ran.load(Ordering::SeqCst));
    assert!(queue.has_failed());
    assert!(factory.has_failed());
}

// A real asynchronous source exercises engine-created queues, not only public
// products. It remains a test runtime, without platform priority tuning.
struct WorkerQueue {
    name: Vec<u8>,
    sender: Mutex<Option<std::sync::mpsc::Sender<(TaskQueueTask, Option<std::time::Instant>)>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    deleted: Arc<AtomicUsize>,
}
impl TaskQueueProvider for WorkerQueue {
    fn queue_name(&self) -> Vec<u8> {
        self.name.clone()
    }
    fn post_task(&self, task: TaskQueueTask, _: TaskQueueLocation) {
        self.post_delayed_task(
            task,
            TimeDelta::zero(),
            TaskDelayPrecision::Low,
            TaskQueueLocation,
        );
    }
    fn post_delayed_task(
        &self,
        task: TaskQueueTask,
        delay: TimeDelta,
        _: TaskDelayPrecision,
        _: TaskQueueLocation,
    ) {
        let due = if delay.is_plus_infinity() {
            None
        } else if delay <= TimeDelta::zero() {
            Some(std::time::Instant::now())
        } else {
            std::time::Instant::now().checked_add(std::time::Duration::from_micros(
                delay.microseconds().unwrap() as u64,
            ))
        };
        if let Some(sender) = self.sender.lock().unwrap().as_ref() {
            let _ = sender.send((task, due));
        }
    }
    fn delete(&self) {
        self.sender.lock().unwrap().take();
        if let Some(worker) = self.worker.lock().unwrap().take() {
            worker.join().unwrap();
        }
        self.deleted.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Default)]
struct WorkerFactory {
    created: AtomicUsize,
    deleted: Arc<AtomicUsize>,
}
impl TaskQueueFactoryProvider for WorkerFactory {
    fn create_task_queue(&self, name: &[u8], _: QueuePriority) -> Box<dyn TaskQueueProvider> {
        self.created.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel::<(TaskQueueTask, Option<std::time::Instant>)>();
        let worker = std::thread::spawn(move || {
            let mut pending: Vec<(Option<std::time::Instant>, TaskQueueTask)> = Vec::new();
            loop {
                let now = std::time::Instant::now();
                while let Some(index) = pending
                    .iter()
                    .position(|(due, _)| due.is_some_and(|due| due <= now))
                {
                    let (_, task) = pending.remove(index);
                    task.execute();
                }
                let wait = pending
                    .iter()
                    .filter_map(|(due, _)| *due)
                    .min()
                    .map_or(std::time::Duration::from_secs(3600), |due| {
                        due.saturating_duration_since(std::time::Instant::now())
                    });
                match rx.recv_timeout(wait) {
                    Ok((task, due)) => pending.push((due, task)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Box::new(WorkerQueue {
            name: name.to_vec(),
            sender: Mutex::new(Some(tx)),
            worker: Mutex::new(Some(worker)),
            deleted: self.deleted.clone(),
        })
    }
}

#[test]
fn production_engine_uses_registered_scheduler_and_retires_all_products() {
    let provider = Arc::new(WorkerFactory::default());
    let mut session = ProductionSession::builder(ProductionSessionConfig::default())
        .encoded_audio()
        .task_queue_provider(provider.clone())
        .unwrap()
        .build()
        .unwrap();
    let peer = session
        .create_peer(pulsebeam_webrtc_sys::PeerConfiguration::default())
        .unwrap();
    let source = session.create_opus_source(1).unwrap();
    session
        .publish_opus(
            peer,
            source,
            "voice",
            pulsebeam_webrtc_sys::RtpTransceiverDirection::SendOnly,
        )
        .unwrap();
    // The actor remains transferable even though its SDK products retain this
    // source. Cleanup must use the engine role/queue retirement sequences.
    std::thread::spawn(move || session.shutdown().unwrap())
        .join()
        .unwrap();
    assert!(provider.created.load(Ordering::SeqCst) > 0);
    assert_eq!(
        provider.created.load(Ordering::SeqCst),
        provider.deleted.load(Ordering::SeqCst)
    );
}

#[test]
fn environment_retains_factory_and_actor_rejects_controlled_factories() {
    let provider = Arc::new(Factory::default());
    let weak = Arc::downgrade(&provider);
    let factory = TaskQueueFactory::from_provider(provider.clone()).unwrap();
    let mut environment = Environment::builder()
        .task_queue_factory(&factory)
        .build()
        .unwrap();
    drop(factory);
    drop(provider);
    assert!(weak.upgrade().is_some());
    environment.close();
    assert!(weak.upgrade().is_none());
    let clock = ManualClock::new(std::time::Duration::from_secs(1)).unwrap();
    let controlled = TaskQueueFactory::cooperative(&clock).unwrap();
    assert!(
        ProductionSession::builder(ProductionSessionConfig::default())
            .task_queue_factory(&controlled)
            .build()
            .is_err()
    );
}
