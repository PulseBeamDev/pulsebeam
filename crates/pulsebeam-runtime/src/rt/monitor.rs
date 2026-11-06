use metrics::{Unit, describe_gauge, gauge};
use std::time::Duration;
use tokio::runtime::Handle;

pub async fn monitor(upkeep: impl Fn(), shutdown: tokio_util::sync::CancellationToken) {
    // This task is spawned on the caller/control runtime, and these metric
    // names have always described that runtime. Data-runtime metrics, if
    // added, should use a separate prefix rather than changing this meaning.
    let metrics = Handle::current().metrics();

    describe_gauge!(
        "tokio_active_tasks",
        Unit::Count,
        "Current number of active tasks"
    );
    describe_gauge!(
        "tokio_injection_queue_depth",
        Unit::Count,
        "Current depth of the global injection queue"
    );
    describe_gauge!(
        "tokio_worker_count",
        Unit::Count,
        "Total number of worker threads"
    );
    describe_gauge!(
        "tokio_blocking_threads",
        Unit::Count,
        "Current number of blocking threads"
    );
    describe_gauge!(
        "tokio_idle_blocking_threads",
        Unit::Count,
        "Current number of idle blocking threads"
    );
    describe_gauge!(
        "tokio_spawned_tasks_total",
        Unit::Count,
        "Total number of tasks spawned since runtime start"
    );

    // Worker specific
    describe_gauge!(
        "tokio_worker_park_count",
        Unit::Count,
        "Total number of times this worker parked"
    );
    describe_gauge!(
        "tokio_worker_steal_count",
        Unit::Count,
        "Total number of times this worker stole tasks"
    );
    describe_gauge!(
        "tokio_worker_poll_count",
        Unit::Count,
        "Total number of times this worker polled"
    );
    describe_gauge!(
        "tokio_worker_busy_duration_seconds",
        Unit::Seconds,
        "Total duration this worker has been busy"
    );
    describe_gauge!(
        "tokio_worker_local_queue_depth",
        Unit::Count,
        "Current depth of this worker's local queue"
    );
    describe_gauge!(
        "tokio_worker_mean_poll_time_us",
        Unit::Microseconds,
        "Mean poll time for this worker"
    );

    let mut interval = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = interval.tick() => {}
        }

        // Current State (Gauges)
        gauge!("tokio_num_alive_tasks").set(metrics.num_alive_tasks() as f64);
        gauge!("tokio_injection_queue_depth",).set(metrics.global_queue_depth() as f64);
        gauge!("tokio_blocking_queue_depth",).set(metrics.blocking_queue_depth() as f64);
        gauge!("tokio_worker_count").set(metrics.num_workers() as f64);
        gauge!("tokio_blocking_threads").set(metrics.num_blocking_threads() as f64);
        gauge!("tokio_idle_blocking_threads").set(metrics.num_idle_blocking_threads() as f64);

        // Cumulative Totals (Technically Counters, but we emit as Gauges because
        // Tokio gives us the absolute total, not the delta).
        gauge!("tokio_spawned_tasks_total").set(metrics.spawned_tasks_count() as f64);
        gauge!("tokio_remote_schedule_total").set(metrics.remote_schedule_count() as f64);
        gauge!("tokio_budget_forced_yield_total").set(metrics.budget_forced_yield_count() as f64);

        gauge!("tokio_io_driver_ready_total").set(metrics.io_driver_ready_count() as f64);

        for i in 0..metrics.num_workers() {
            let labels = [("worker", i.to_string())];

            gauge!("tokio_worker_local_queue_depth", &labels,)
                .set(metrics.worker_local_queue_depth(i) as f64);

            gauge!("tokio_worker_park_count", &labels).set(metrics.worker_park_count(i) as f64);

            gauge!("tokio_worker_noop_count", &labels).set(metrics.worker_noop_count(i) as f64);

            gauge!("tokio_worker_steal_count", &labels).set(metrics.worker_steal_count(i) as f64);

            gauge!("tokio_worker_poll_count", &labels).set(metrics.worker_poll_count(i) as f64);

            gauge!("tokio_worker_overflow_count", &labels)
                .set(metrics.worker_overflow_count(i) as f64);

            gauge!("tokio_worker_busy_duration_seconds", &labels)
                .set(metrics.worker_total_busy_duration(i).as_secs_f64());

            gauge!("tokio_worker_mean_poll_time_us", &labels)
                .set(metrics.worker_mean_poll_time(i).as_micros() as f64);
        }

        upkeep();
    }
}
