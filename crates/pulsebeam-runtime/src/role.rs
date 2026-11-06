//! Owned low-priority execution role for optional background I/O.

use std::{future::Future, io};
use tokio_util::sync::CancellationToken;

struct CancelOnExit(CancellationToken);
impl Drop for CancelOnExit {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// An owned role. Dropping it requests shutdown; `finish` also joins it.
pub struct IdleRole {
    shutdown: CancellationToken,
    #[cfg(not(feature = "sim"))]
    thread: Option<std::thread::JoinHandle<io::Result<()>>>,
    #[cfg(feature = "sim")]
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for IdleRole {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl IdleRole {
    /// Initialize on the role's executor, publish readiness, then run until stopped.
    /// Simulation initializes and runs on the caller's runtime without a native thread.
    pub async fn start<T, Init, InitFuture, Run>(
        name: &str,
        spare_core: Option<usize>,
        shutdown: CancellationToken,
        init: Init,
    ) -> io::Result<(Self, T)>
    where
        T: Send + 'static,
        Init: FnOnce() -> InitFuture + Send + 'static,
        InitFuture: Future<Output = io::Result<(T, Run)>> + Send + 'static,
        Run: Future<Output = ()> + Send + 'static,
    {
        #[cfg(feature = "sim")]
        {
            let _ = (name, spare_core);
            let (ready, run) = init().await?;
            let stop = shutdown.clone();
            let task = tokio::spawn(async move {
                let _cancel = CancelOnExit(stop);
                run.await;
            });
            Ok((
                Self {
                    shutdown,
                    task: Some(task),
                },
                ready,
            ))
        }
        #[cfg(not(feature = "sim"))]
        {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let stop = shutdown.clone();
            let thread = std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    let _cancel = CancelOnExit(stop);
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(async move {
                        let initialized = match tune_thread(spare_core) {
                            Ok(()) => init().await,
                            Err(error) => Err(error),
                        };
                        match initialized {
                            Ok((ready, run)) => {
                                if ready_tx.send(Ok(ready)).is_ok() {
                                    run.await;
                                }
                            }
                            Err(error) => {
                                let _ = ready_tx.send(Err(error));
                            }
                        }
                        Ok(())
                    })
                })?;
            let role = Self {
                shutdown,
                thread: Some(thread),
            };
            match ready_rx.await {
                Ok(Ok(ready)) => Ok((role, ready)),
                result => {
                    let startup_error = match result {
                        Ok(Err(error)) => error,
                        _ => io::Error::other("role failed at startup"),
                    };
                    // Joining here ensures failed initialization never leaves a detached thread.
                    role.finish().await?;
                    Err(startup_error)
                }
            }
        }
    }

    pub async fn finish(mut self) -> io::Result<()> {
        self.shutdown.cancel();
        #[cfg(feature = "sim")]
        if let Some(task) = self.task.take() {
            task.await.map_err(io::Error::other)?;
        }
        #[cfg(not(feature = "sim"))]
        if let Some(thread) = self.thread.take() {
            tokio::task::spawn_blocking(move || {
                thread
                    .join()
                    .map_err(|_| io::Error::other("role thread panicked"))?
            })
            .await
            .map_err(io::Error::other)??;
        }
        Ok(())
    }
}

#[cfg(not(feature = "sim"))]
fn tune_thread(spare_core: Option<usize>) -> io::Result<()> {
    use thread_priority::{
        NormalThreadSchedulePolicy, ThreadPriority, ThreadSchedulePolicy, thread_native_id,
    };
    thread_priority::set_thread_priority_and_policy(
        thread_native_id(),
        ThreadPriority::Min,
        ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Idle),
    )
    .map_err(io::Error::other)?;
    if let Some(core) = spare_core
        && !core_affinity::set_for_current(core_affinity::CoreId { id: core })
    {
        tracing::warn!(core, "role spare-core affinity unavailable");
    }
    tracing::info!(?spare_core, "role thread uses SCHED_IDLE");
    Ok(())
}

/// CPU time consumed by the current role thread, or zero in simulation.
pub fn thread_cpu_nanos() -> u64 {
    #[cfg(not(feature = "sim"))]
    {
        let time = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
        u64::try_from(time.tv_sec)
            .unwrap_or_default()
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(time.tv_nsec).unwrap_or_default())
    }
    #[cfg(feature = "sim")]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn readiness_and_joined_shutdown() {
        crate::testing::run_local(crate::testing::test_host_ip("192.168.250.17"), async {
            let shutdown = CancellationToken::new();
            let stop = shutdown.clone();
            let (finished, received) = tokio::sync::oneshot::channel();
            let (role, ready) =
                IdleRole::start("test-idle-role", None, shutdown, move || async move {
                    Ok((7, async move {
                        stop.cancelled().await;
                        let _ = finished.send(());
                    }))
                })
                .await
                .unwrap();
            assert_eq!(ready, 7);
            role.finish().await.unwrap();
            received.await.unwrap();
        });
    }

    #[test]
    fn initialization_error_is_returned() {
        crate::testing::run_local(crate::testing::test_host_ip("192.168.250.18"), async {
            let result =
                IdleRole::start("test-idle-role", None, CancellationToken::new(), || async {
                    Err::<((), std::future::Ready<()>), _>(io::Error::other(
                        "initialization failed",
                    ))
                })
                .await;
            assert_eq!(result.err().unwrap().to_string(), "initialization failed");
        });
    }
}
