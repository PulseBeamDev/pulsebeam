use super::{ExecutionConfig, ManagedService, Profile, Service, managed::ServiceFuture};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub struct ServiceSnapshot {
    pub id: usize,
    pub generation: u64,
    pub at: Instant,
    /// Poll wall time accumulated by this envelope; restarts after handoff or rollback.
    pub busy: Duration,
    /// Runnable wait accumulated by the pinned service; survives handoff.
    pub scheduling_wait: Duration,
}

#[derive(Debug)]
pub struct WorkerSnapshot {
    pub worker: usize,
    pub epoch: u64,
    pub services: Vec<ServiceSnapshot>,
    pub scheduling_wait: Duration,
}

/// A complete observation, invalidated while a service changes workers.
#[derive(Debug)]
pub struct ExecutionSnapshot {
    pub epoch: u64,
    pub at: Instant,
    pub workers: Vec<WorkerSnapshot>,
}

/// Bounded execution control without an autonomous placement policy.
#[derive(Clone)]
pub struct MigrationHandle {
    requests: mpsc::Sender<Relocation>,
    observed: watch::Receiver<Option<ExecutionSnapshot>>,
}

pub struct Migration {
    completed: oneshot::Receiver<io::Result<()>>,
}

impl Migration {
    pub fn try_complete(&mut self) -> io::Result<bool> {
        match self.completed.try_recv() {
            Ok(result) => result.map(|()| true),
            Err(oneshot::error::TryRecvError::Empty) => Ok(false),
            Err(error) => Err(io::Error::other(error)),
        }
    }
}

impl MigrationHandle {
    /// Borrow the latest complete observation without maintaining another residency map.
    pub fn with_snapshot<R>(&self, read: impl FnOnce(Option<&ExecutionSnapshot>) -> R) -> R {
        read(self.observed.borrow().as_ref())
    }

    pub fn try_move_service(&self, id: usize, destination: usize) -> io::Result<Migration> {
        let (reply, completed) = oneshot::channel();
        self.requests
            .try_send(Relocation {
                id,
                destination,
                reply,
            })
            .map_err(|_| io::Error::other("runtime relocation control unavailable"))?;
        Ok(Migration { completed })
    }

    pub async fn move_service(&self, id: usize, destination: usize) -> io::Result<()> {
        let mut observed = self.observed.clone();
        loop {
            observed.borrow_and_update();
            let migration = self.try_move_service(id, destination)?;
            match migration.completed.await.map_err(io::Error::other)? {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    observed.changed().await.map_err(io::Error::other)?;
                }
                result => return result,
            }
        }
    }
}

struct Relocation {
    id: usize,
    destination: usize,
    reply: oneshot::Sender<io::Result<()>>,
}
struct Pending {
    id: usize,
    source: usize,
    destination: usize,
    generation: u64,
    reply: oneshot::Sender<io::Result<()>>,
}

#[cfg(not(feature = "sim"))]
type Factory = Box<dyn FnOnce() -> io::Result<ServiceFuture> + Send>;
const CONTROL_CAPACITY: usize = 64;

struct MoveRequest {
    generation: u64,
    slot: OwnedSemaphorePermit,
    delivery: mpsc::OwnedPermit<Command>,
    rollback: mpsc::OwnedPermit<Command>,
}

enum Request {
    Move(MoveRequest),
    Sample(oneshot::Sender<ServiceSnapshot>),
}

struct Arrival {
    id: usize,
    generation: u64,
    future: ServiceFuture,
    slot: OwnedSemaphorePermit,
    rollback: Option<(usize, OwnedSemaphorePermit, mpsc::OwnedPermit<Command>)>,
}

enum Command {
    #[cfg(not(feature = "sim"))]
    Start {
        id: usize,
        factory: Factory,
        slot: OwnedSemaphorePermit,
        ready: oneshot::Sender<io::Result<()>>,
    },
    Arrive(Arrival),
    Move {
        id: usize,
        request: MoveRequest,
    },
    Snapshot(u64),
}

enum Event {
    Snapshot(WorkerSnapshot),
    Moved {
        id: usize,
        source: usize,
        destination: usize,
        generation: u64,
    },
    Rejected {
        id: usize,
        generation: u64,
    },
}

struct Resident {
    generation: u64,
    moving: bool,
    requests: mpsc::Sender<Request>,
}

struct Departure {
    id: usize,
    future: ServiceFuture,
    slot: OwnedSemaphorePermit,
    request: Option<MoveRequest>,
    result: io::Result<()>,
}

struct Measured<'a> {
    future: &'a mut ServiceFuture,
    busy: &'a mut Duration,
}

impl Future for Measured<'_> {
    type Output = io::Result<()>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let start = Instant::now();
        let result = self.future.as_mut().poll(cx);
        *self.busy = self.busy.saturating_add(start.elapsed());
        result
    }
}

async fn envelope(
    mut arrival: Arrival,
    mut requests: mpsc::Receiver<Request>,
    shutdown: CancellationToken,
) -> Departure {
    let mut busy = Duration::ZERO;
    let (request, result) = loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break (None, Ok(())),
            request = requests.recv() => match request {
                Some(Request::Sample(reply)) => {
                    let _ = reply.send(ServiceSnapshot {
                        id: arrival.id,
                        generation: arrival.generation,
                        at: Instant::now(),
                        busy,
                        scheduling_wait: arrival.future.as_ref().scheduling_wait(),
                    });
                }
                Some(Request::Move(request)) => {
                    let result = arrival.future.as_mut().detach();
                    break (Some(request), result);
                }
                None => break (None, Err(io::Error::other("service control channel closed"))),
            },
            result = (Measured { future: &mut arrival.future, busy: &mut busy }) => break (None, result),
        }
    };
    Departure {
        id: arrival.id,
        future: arrival.future,
        slot: arrival.slot,
        request,
        result,
    }
}

async fn run_current(
    future: impl Future<Output = io::Result<()>>,
    stop: CancellationToken,
) -> io::Result<()> {
    tokio::select! {
        biased;
        _ = stop.cancelled() => Ok(()),
        result = future => {
            stop.cancel();
            result.and_then(|_| Err(io::Error::other("managed service exited before node shutdown")))
        }
    }
}

fn install(
    arrival: Arrival,
    residents: &mut HashMap<usize, Resident>,
    tasks: &mut JoinSet<Departure>,
    shutdown: &CancellationToken,
) {
    let (requests, receiver) = mpsc::channel(2);
    residents.insert(
        arrival.id,
        Resident {
            generation: arrival.generation,
            moving: false,
            requests,
        },
    );
    tasks.spawn(envelope(arrival, receiver, shutdown.clone()));
}

async fn worker_loop(
    worker: usize,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    struct CancelOnExit(CancellationToken);
    impl Drop for CancelOnExit {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _cancel_on_exit = CancelOnExit(shutdown.clone());
    let mut residents = HashMap::<usize, Resident>::new();
    let mut retired = HashMap::<usize, u64>::new();
    let mut tasks = JoinSet::<Departure>::new();
    let mut samples = JoinSet::<Option<WorkerSnapshot>>::new();
    let result = loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break Ok(()),
            Some(result) = tasks.join_next() => {
                let departure = match result {
                    Ok(departure) => departure,
                    Err(error) => break Err(io::Error::other(error)),
                };
                let Some(resident) = residents.remove(&departure.id) else { break Err(io::Error::other("departing service has no resident owner")); };
                if let Err(error) = departure.result {
                    if departure.request.is_some() {
                        let mut future = departure.future;
                        if let Err(restore) = future.as_mut().attach() { break Err(io::Error::other(format!("source detach failed ({error}) and restoration failed: {restore}"))); }
                        install(Arrival { id: departure.id, generation: resident.generation, future, slot: departure.slot, rollback: None }, &mut residents, &mut tasks, &shutdown);
                        let generation = departure.request.as_ref().map_or(0, |request| request.generation);
                        let _ = events.send(Event::Rejected { id: departure.id, generation }).await;
                        continue;
                    }
                    break Err(error);
                }
                if let Some(request) = departure.request {
                    retired.insert(departure.id, request.generation);
                    request.delivery.send(Command::Arrive(Arrival {
                        id: departure.id, generation: request.generation,
                        future: departure.future, slot: request.slot,
                        rollback: Some((worker, departure.slot, request.rollback)),
                    }));
                } else if !shutdown.is_cancelled() {
                    break Err(io::Error::other("managed service exited before node shutdown"));
                }
            }
            Some(result) = samples.join_next() => {
                match result {
                    Ok(Some(snapshot)) => { if events.send(Event::Snapshot(snapshot)).await.is_err() { break Err(io::Error::other("execution coordinator stopped")); } }
                    Ok(None) => {}
                    Err(error) => break Err(io::Error::other(error)),
                }
            }
            command = commands.recv() => match command {
                #[cfg(not(feature = "sim"))]
                Some(Command::Start {id, factory, slot, ready}) => {
                    if residents.contains_key(&id) {
                        let _ = ready.send(Err(io::Error::other("duplicate service identity")));
                        continue;
                    }
                    match factory() {
                        Ok(future) => {
                            install(Arrival { id, generation: 0, future, slot, rollback: None }, &mut residents, &mut tasks, &shutdown);
                            let _ = ready.send(Ok(()));
                        }
                        Err(error) => { let _ = ready.send(Err(error)); }
                    }
                }
                Some(Command::Arrive(mut arrival)) => {
                    retired.remove(&arrival.id);
                    if residents.contains_key(&arrival.id) { break Err(io::Error::other("duplicate service owner at destination")); }
                    match arrival.future.as_mut().attach() {
                        Ok(()) => {
                            if let Some((source, _, _)) = arrival.rollback.take() {
                                if events.send(Event::Moved { id: arrival.id, source, destination: worker, generation: arrival.generation }).await.is_err() {
                                    break Err(io::Error::other("execution coordinator stopped during transfer"));
                                }
                            } else if events.send(Event::Rejected { id: arrival.id, generation: arrival.generation }).await.is_err() {
                                break Err(io::Error::other("execution coordinator stopped during rollback"));
                            }
                            install(arrival, &mut residents, &mut tasks, &shutdown);
                        }
                        Err(error) => {
                            if let Some((_source, slot, rollback)) = arrival.rollback.take() {
                                arrival.slot = slot;
                                rollback.send(Command::Arrive(arrival));
                            } else { break Err(io::Error::other(format!("service registration and rollback failed: {error}"))); }
                        }
                    }
                }
                Some(Command::Move {id, request}) => {
                    let generation = request.generation;
                    if retired.get(&id).is_some_and(|retired| generation <= *retired)
                        || residents.get(&id).is_some_and(|resident| resident.moving && resident.generation == generation) { continue; }
                    let valid = residents.get(&id).is_some_and(|resident| !resident.moving && resident.generation.checked_add(1) == Some(request.generation));
                    if valid {
                        if let Some(resident) = residents.get_mut(&id) {
                            resident.generation = generation;
                            if resident.requests.try_send(Request::Move(request)).is_err() { let _ = events.send(Event::Rejected { id, generation }).await; }
                            else { resident.moving = true; }
                        }
                    } else { let _ = events.send(Event::Rejected { id, generation }).await; }
                }
                Some(Command::Snapshot(epoch)) => {
                    if !samples.is_empty() { continue; }
                    let mut replies = Vec::with_capacity(residents.len());
                    let mut complete = true;
                    for resident in residents.values() {
                        let (reply, receiver) = oneshot::channel();
                        if resident.requests.try_send(Request::Sample(reply)).is_ok() { replies.push(receiver); }
                        else { complete = false; break; }
                    }
                    if !complete { continue; }
                    samples.spawn(async move {
                        let mut services = Vec::with_capacity(replies.len());
                        let mut scheduling_wait = Duration::ZERO;
                        for receiver in replies {
                            let Ok(sample) = receiver.await else { return None; };
                            scheduling_wait = scheduling_wait.saturating_add(sample.scheduling_wait);
                            services.push(sample);
                        }
                        Some(WorkerSnapshot { worker, epoch, services, scheduling_wait })
                    });
                }
                None => break Ok(()),
            }
        }
    };
    shutdown.cancel();
    commands.close();
    while commands.try_recv().is_ok() {}
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    samples.abort_all();
    while samples.join_next().await.is_some() {}
    result
}

struct Worker {
    commands: mpsc::Sender<Command>,
    slots: std::sync::Arc<Semaphore>,
    thread: Option<std::thread::JoinHandle<io::Result<()>>>,
}

/// Owns data workers, service tasks, execution control and their complete joins.
pub struct DataExecutor {
    config: ExecutionConfig,
    started: HashSet<usize>,
    control: Option<MigrationHandle>,
    workers: Vec<Worker>,
    local: JoinSet<io::Result<()>>,
    coordinator: Option<tokio::task::JoinHandle<io::Result<()>>>,
    shutdown: CancellationToken,
}

impl Drop for DataExecutor {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl DataExecutor {
    pub async fn new(config: ExecutionConfig, shutdown: CancellationToken) -> io::Result<Self> {
        let shards = config.shard_count()?;
        #[cfg(feature = "sim")]
        if !config.is_current_runtime() {
            return Err(io::Error::other(
                "a simulated node must use .with_current_runtime()",
            ));
        }
        let mut executor = Self {
            config,
            started: HashSet::new(),
            control: None,
            workers: Vec::new(),
            local: JoinSet::new(),
            coordinator: None,
            shutdown,
        };
        if config.is_current_runtime() {
            return Ok(executor);
        }
        let count = if config.profile == Profile::WorkStealing {
            1
        } else {
            config.workers
        };
        let (events, receiver) = mpsc::channel(CONTROL_CAPACITY);
        let cores = core_affinity::get_core_ids().unwrap_or_default();
        for index in 0..count {
            let (commands, incoming) = mpsc::channel(CONTROL_CAPACITY);
            let slots = std::sync::Arc::new(Semaphore::new(shards));
            let (ready, started) = oneshot::channel();
            let stop = executor.shutdown.clone();
            let events = events.clone();
            let core = index
                .checked_rem(cores.len())
                .and_then(|i| cores.get(i))
                .copied();
            let core_ids = cores.clone();
            let thread = std::thread::Builder::new()
                .name(format!("pb-data-{index}"))
                .spawn(move || {
                    let mut builder = if config.profile == Profile::WorkStealing {
                        let mut builder = tokio::runtime::Builder::new_multi_thread();
                        builder.worker_threads(config.workers).thread_name("pb-data").disable_lifo_slot();
                        let next = std::sync::atomic::AtomicUsize::new(0);
                        builder.on_thread_start(move || {
                            let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if index < config.workers {
                                let core = index.checked_rem(core_ids.len()).and_then(|i| core_ids.get(i)).copied();
                                let realtime = super::process::tune_data_thread(core);
                                metrics::gauge!("data_worker_realtime", "worker" => index.to_string()).set(f64::from(realtime));
                            }
                        });
                        builder
                    } else {
                        tokio::runtime::Builder::new_current_thread()
                    };
                    let runtime = match builder.enable_all().enable_alt_timer().build() {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let _ = ready.send(Err(error));
                            return Ok(());
                        }
                    };
                    if config.profile != Profile::WorkStealing {
                        let realtime = super::process::tune_data_thread(core);
                        metrics::gauge!("data_worker_realtime", "worker" => index.to_string()).set(f64::from(realtime));
                    }
                    if ready.send(Ok(())).is_err() {
                        return Ok(());
                    }
                    runtime.block_on(worker_loop(index, incoming, events, stop))
                });
            let thread = match thread {
                Ok(thread) => thread,
                Err(error) => {
                    executor.finish().await?;
                    return Err(error);
                }
            };
            executor.workers.push(Worker {
                commands,
                slots,
                thread: Some(thread),
            });
            match started.await {
                Ok(Ok(())) => {}
                result => {
                    let error = match result {
                        Ok(Err(error)) => error,
                        _ => io::Error::other("data worker failed at startup"),
                    };
                    executor.finish().await?;
                    return Err(error);
                }
            }
        }
        if cfg!(target_os = "linux") && config.profile == Profile::Adaptive && config.workers > 1 {
            let (requests, relocations) = mpsc::channel(1);
            let (observations, observed) = watch::channel(None);
            executor.control = Some(MigrationHandle { requests, observed });
            let commands = executor
                .workers
                .iter()
                .map(|w| w.commands.clone())
                .collect();
            let slots = executor.workers.iter().map(|w| w.slots.clone()).collect();
            executor.coordinator = Some(tokio::spawn(coordinate_execution(
                commands,
                slots,
                receiver,
                relocations,
                observations,
                executor.shutdown.clone(),
            )));
        } else {
            executor.coordinator = Some(tokio::spawn(async move {
                let mut receiver = receiver;
                while receiver.recv().await.is_some() {}
                Ok(())
            }));
        }
        Ok(executor)
    }

    #[cfg(not(feature = "sim"))]
    pub async fn spawn_on<S: Service + Send>(
        &mut self,
        id: usize,
        worker: usize,
        launch: impl FnOnce() -> io::Result<ManagedService<S>> + Send + 'static,
    ) -> io::Result<()> {
        self.check_identity(id)?;
        self.check_worker(worker)?;
        if self.config.is_current_runtime() {
            let future = launch()?;
            let stop = self.shutdown.clone();
            self.local.spawn_local(run_current(future, stop));
        } else {
            self.spawn_factory(id, worker, Box::new(move || Ok(Box::pin(launch()?))))
                .await?;
        }
        self.started.insert(id);
        Ok(())
    }

    #[cfg(feature = "sim")]
    pub async fn spawn_on<S: Service>(
        &mut self,
        id: usize,
        worker: usize,
        launch: impl FnOnce() -> io::Result<ManagedService<S>> + 'static,
    ) -> io::Result<()> {
        self.check_identity(id)?;
        self.check_worker(worker)?;
        let future = launch()?;
        let stop = self.shutdown.clone();
        self.local.spawn_local(run_current(future, stop));
        self.started.insert(id);
        Ok(())
    }

    fn check_worker(&self, worker: usize) -> io::Result<()> {
        if worker < self.worker_count() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown data worker",
            ))
        }
    }

    fn check_identity(&self, id: usize) -> io::Result<()> {
        // Identity remains reserved when the service leaves its initial worker.
        if self.started.contains(&id) {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "duplicate service identity",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(not(feature = "sim"))]
    async fn spawn_factory(&self, id: usize, worker: usize, factory: Factory) -> io::Result<()> {
        let worker = self
            .workers
            .get(worker)
            .ok_or_else(|| io::Error::other("no data worker"))?;
        let slot = worker
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(io::Error::other)?;
        let (ready, started) = oneshot::channel();
        worker
            .commands
            .send(Command::Start {
                id,
                factory,
                slot,
                ready,
            })
            .await
            .map_err(|_| io::Error::other("data worker stopped"))?;
        started.await.map_err(io::Error::other)?
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len().max(1)
    }

    pub fn migration_handle(&self) -> Option<MigrationHandle> {
        self.control.clone()
    }

    pub fn should_tune_control_thread(&self) -> bool {
        !self.config.is_current_runtime()
    }

    /// Joining acknowledges service/socket destruction, including queued transfers.
    pub async fn finish(mut self) -> io::Result<()> {
        self.shutdown.cancel();
        let mut local_error = None;
        while let Some(result) = self.local.join_next().await {
            if let Err(error) = result.map_err(io::Error::other).and_then(|result| result) {
                local_error.get_or_insert(error);
            }
        }
        let threads: Vec<_> = self
            .workers
            .iter_mut()
            .filter_map(|worker| worker.thread.take())
            .collect();
        let joined = tokio::task::spawn_blocking(move || {
            let mut error = None;
            for thread in threads {
                match thread.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        error.get_or_insert(e);
                    }
                    Err(_) => {
                        error.get_or_insert_with(|| io::Error::other("data worker panicked"));
                    }
                }
            }
            error.map_or(Ok(()), Err)
        })
        .await
        .map_err(io::Error::other)?;
        // Close command channels before joining the non-adaptive event drain.
        self.workers.clear();
        if let Some(coordinator) = self.coordinator.take() {
            coordinator.await.map_err(io::Error::other)??;
        }
        joined?;
        local_error.map_or(Ok(()), Err)
    }
}

#[cfg(all(test, target_os = "linux", not(feature = "sim")))]
#[path = "executor_tests.rs"]
mod tests;

fn request_move(
    id: usize,
    source: usize,
    destination: usize,
    generation: u64,
    commands: &[mpsc::Sender<Command>],
    slots: &[std::sync::Arc<Semaphore>],
) -> io::Result<()> {
    if source == destination {
        return Err(io::Error::other("service already resides on destination"));
    }
    let dest = commands
        .get(destination)
        .ok_or_else(|| io::Error::other("unknown destination worker"))?;
    let src = commands
        .get(source)
        .ok_or_else(|| io::Error::other("unknown source worker"))?;
    let slot = slots
        .get(destination)
        .ok_or_else(|| io::Error::other("unknown destination capacity"))?
        .clone()
        .try_acquire_owned()
        .map_err(io::Error::other)?;
    let delivery = dest.clone().try_reserve_owned().map_err(io::Error::other)?;
    let rollback = src.clone().try_reserve_owned().map_err(io::Error::other)?;
    src.try_send(Command::Move {
        id,
        request: MoveRequest {
            generation,
            slot,
            delivery,
            rollback,
        },
    })
    .map_err(|_| io::Error::other("source worker control unavailable"))?;
    Ok(())
}

async fn coordinate_execution(
    commands: Vec<mpsc::Sender<Command>>,
    slots: Vec<std::sync::Arc<Semaphore>>,
    mut events: mpsc::Receiver<Event>,
    mut relocations: mpsc::Receiver<Relocation>,
    observations: watch::Sender<Option<ExecutionSnapshot>>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut epoch = 0u64;
    let mut snapshots = HashMap::new();
    let mut generations = HashMap::<usize, u64>::new();
    let mut locations = HashMap::<usize, usize>::new();
    let mut cohort_ready = false;
    let mut pending: Option<Pending> = None;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            relocation = relocations.recv(), if !relocations.is_closed() => {
                if let Some(Relocation {id, destination, reply}) = relocation {
                    if pending.is_some() {
                        let _ = reply.send(Err(io::Error::other("a service move is already outstanding")));
                        continue;
                    }
                    let Some(&source) = locations.get(&id) else {
                        let kind = if cohort_ready { io::ErrorKind::NotFound } else { io::ErrorKind::WouldBlock };
                        let _ = reply.send(Err(io::Error::new(kind, "service has no fresh placement observation")));
                        continue;
                    };
                    let Some(generation) = generations.get(&id).copied().unwrap_or(0).checked_add(1) else {
                        let _ = reply.send(Err(io::Error::other("migration generation exhausted")));
                        continue;
                    };
                    match request_move(id, source, destination, generation, &commands, &slots) {
                        Ok(()) => {
                            generations.insert(id, generation);
                            pending = Some(Pending { id, source, destination, generation, reply });
                            observations.send_replace(None);
                        }
                        Err(error) => { let _ = reply.send(Err(error)); }
                    }
                }
            }
            event = events.recv() => match event {
                Some(Event::Snapshot(snapshot)) if snapshot.epoch == epoch && pending.is_none() => {
                    metrics::gauge!("runtime_worker_runnable_wait_seconds", "worker" => snapshot.worker.to_string()).set(snapshot.scheduling_wait.as_secs_f64());
                    snapshots.insert(snapshot.worker, snapshot);
                    if snapshots.len() == commands.len() {
                        let cohort: Vec<_> = (0..commands.len()).filter_map(|i| snapshots.remove(&i)).collect();
                        locations.clear();
                        for snapshot in &cohort {
                            for service in &snapshot.services { locations.insert(service.id, snapshot.worker); }
                        }
                        cohort_ready = true;
                        observations.send_replace(Some(ExecutionSnapshot { epoch, at: Instant::now(), workers: cohort }));
                    }
                }
                Some(Event::Snapshot(_)) => {}
                Some(Event::Moved {id, source, destination, generation}) => {
                    let Some(move_request) = pending.take() else {
                        shutdown.cancel();
                        return Err(io::Error::other("migration completed without a request"));
                    };
                    if (move_request.id, move_request.source, move_request.destination, move_request.generation)
                        != (id, source, destination, generation) {
                        shutdown.cancel();
                        return Err(io::Error::other("stale or conflicting migration completion"));
                    }
                    generations.insert(id, generation);
                    locations.clear();
                    cohort_ready = false;
                    observations.send_replace(None);
                    let _ = move_request.reply.send(Ok(()));
                    metrics::counter!("runtime_shard_moves").increment(1);
                }
                Some(Event::Rejected { id, generation }) => {
                    if !pending.as_ref().is_some_and(|request| request.id == id && request.generation == generation) { continue; }
                    if let Some(request) = pending.take() {
                        let _ = request.reply.send(Err(io::Error::other("service relocation rejected or rolled back")));
                    }
                    locations.clear();
                    cohort_ready = false;
                    snapshots.clear();
                    observations.send_replace(None);
                }
                None => { shutdown.cancel(); return Err(io::Error::other("all data worker event channels closed")); }
            },
            _ = interval.tick(), if pending.is_none() => {
                epoch = epoch.saturating_add(1);
                snapshots.clear();
                locations.clear();
                cohort_ready = false;
                for sender in &commands {
                    if sender.try_send(Command::Snapshot(epoch)).is_err() { snapshots.clear(); break; }
                }
            }
        }
    }
}
