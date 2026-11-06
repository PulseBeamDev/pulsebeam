use std::{future::Future, io};

pub fn spare_core(data_workers: usize) -> Option<usize> {
    core_affinity::get_core_ids().and_then(|cores| cores.get(data_workers).map(|core| core.id))
}

pub fn available_workers() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// Enter the process control role. Backend handles never escape to application policy.
#[allow(
    clippy::disallowed_methods,
    reason = "process bootstrap enters its owned runtime once"
)]
pub fn block_on<F: Future>(future: F) -> io::Result<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .enable_alt_timer()
        .build_local(tokio::runtime::LocalOptions::default())?;
    Ok(runtime.block_on(future))
}

pub fn tune_control_thread() {
    #[cfg(unix)]
    {
        use thread_priority::{
            NormalThreadSchedulePolicy, ThreadPriority, ThreadSchedulePolicy, thread_native_id,
        };

        let current_thread_id = thread_native_id();

        #[cfg(target_os = "linux")]
        let policy = ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Batch);

        #[cfg(not(target_os = "linux"))]
        let policy = ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Other);

        let result = thread_priority::set_thread_priority_and_policy(
            current_thread_id,
            ThreadPriority::Min,
            policy,
        );

        if let Err(e) = result {
            tracing::warn!("Failed to lower Control Thread priority: {:?}", e);
        } else {
            tracing::debug!("Control thread tuned: Minimum Priority");
        }
    }

    #[cfg(not(unix))]
    {
        // Fallback for Windows or other non-Unix targets if necessary
        tracing::debug!("Thread tuning is a no-op on non-Unix platforms.");
    }
}

pub fn tune_data_thread(core_id: Option<core_affinity::CoreId>) -> bool {
    #[cfg(target_os = "linux")]
    {
        use rustix::thread::{current_timer_slack, set_current_timer_slack};
        use std::num::NonZero;
        use thread_priority::{
            RealtimeThreadSchedulePolicy, ScheduleParams, ThreadPriority, ThreadSchedulePolicy,
            thread_native_id,
        };

        let current_thread_id = thread_native_id();
        // small enough priority to avoid inversion with IRQ
        let policy = ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo);
        let priority = ThreadPriority::from_posix(ScheduleParams { sched_priority: 10 });
        let realtime = if let Err(e) =
            thread_priority::set_thread_priority_and_policy(current_thread_id, priority, policy)
        {
            tracing::warn!(
                "Failed to set Data Thread to SCHED_FIFO at priority 10 (requires CAP_SYS_NICE): {:?}",
                e
            );
            false
        } else {
            tracing::debug!("Data thread successfully elevated to SCHED_FIFO (Priority 10)");
            true
        };

        // attempt to get closer to SCHED_FIFO without CAP_SYS_ADMIN
        let slack_value = NonZero::new(1);
        if let Err(err) = set_current_timer_slack(slack_value) {
            tracing::warn!(?err, "Failed to set timer slack on data thread");
        } else {
            let current_slack = current_timer_slack().unwrap_or(0);
            tracing::debug!(current_slack, "Data thread timer slack successfully tuned");
        }

        // https://developers.redhat.com/articles/2025/03/26/rhel-real-time-cpu-throttling-and-risks#is_that_a_bug_
        // set higher priority first before pinning to avoid a potential lockup
        if let Some(core) = core_id {
            if core_affinity::set_for_current(core) {
                tracing::debug!(?core, "Data thread pinned to CPU core");
            } else {
                tracing::warn!(?core, "Failed to pin Data thread to CPU core");
            }
        }
        realtime
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = core_id;
        false
    }
}
