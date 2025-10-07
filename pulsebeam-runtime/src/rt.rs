use std::time::Duration;

pub use tokio::runtime::Handle;
pub use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};
use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot};

pub async fn yield_now() {
    tokio::task::yield_now().await;
}

pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await
}

type BoxedFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

enum WorkerMsg {
    Spawn(BoxedFuture),
}

struct Worker {
    tx: mpsc::UnboundedSender<WorkerMsg>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// A handle to a spawned task.
pub struct JoinHandle<T> {
    rx: oneshot::Receiver<T>,
    cancel_token: CancellationToken,
}

impl<T> JoinHandle<T> {
    /// Abort the task
    pub fn abort(&self) {
        self.cancel_token.cancel();
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, oneshot::error::RecvError>;
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        Pin::new(&mut self.rx).poll(cx)
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Pulsebeam runtime: N worker threads, each with a single-threaded Tokio runtime
pub struct PulsebeamRuntime {
    workers: Vec<Worker>,
    next: AtomicUsize,
}

impl PulsebeamRuntime {
    /// Create a new runtime with `num_workers` threads
    pub fn new(num_workers: usize) -> Arc<Self> {
        assert!(num_workers > 0);
        let mut workers = Vec::with_capacity(num_workers);

        for i in 0..num_workers {
            let (tx, mut rx) = mpsc::unbounded_channel::<WorkerMsg>();
            let name = format!("pulsebeam-worker-{}", i);

            let handle = thread::Builder::new()
                .name(name)
                .spawn(move || {
                    // Single-threaded runtime per worker
                    let rt = Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to build runtime");

                    rt.block_on(async move {
                        while let Some(msg) = rx.recv().await {
                            match msg {
                                WorkerMsg::Spawn(task) => {
                                    tokio::spawn(task); // Send tasks can migrate
                                }
                            }
                        }
                    });
                })
                .expect("failed to spawn worker");

            workers.push(Worker {
                tx,
                thread: Some(handle),
            });
        }

        Arc::new(Self {
            workers,
            next: AtomicUsize::new(0),
        })
    }

    /// Spawn a `Send` task on a specific worker thread
    pub fn spawn_on<F, T>(&self, idx: usize, fut: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let cancel_token = CancellationToken::new();
        let child_token = cancel_token.child_token();

        let (tx, rx) = oneshot::channel::<T>();
        let wrapper = async move {
            tokio::select! {
                _ = child_token.cancelled() => {}
                res = fut => { let _ = tx.send(res); }
            }
        };
        let worker = &self.workers[idx % self.workers.len()];
        worker
            .tx
            .send(WorkerMsg::Spawn(Box::pin(wrapper)))
            .expect("worker closed");

        JoinHandle { rx, cancel_token }
    }

    /// Spawn a `Send` task using round-robin scheduling
    pub fn spawn<F, T>(&self, fut: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let idx = self.next.fetch_add(1, Ordering::Relaxed);
        self.spawn_on(idx % self.workers.len(), fut)
    }

    /// Synchronously block on a future, running it on one of the worker threads
    pub fn block_on<F, T>(&self, fut: F) -> T
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();

        let _guard = self.spawn(async move {
            let res = fut.await;
            let _ = tx.send(res);
        });

        rx.blocking_recv().expect("task was cancelled")
    }
}
