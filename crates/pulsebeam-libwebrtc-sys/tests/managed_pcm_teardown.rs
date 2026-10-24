//! Actual stock audio graph retirement while a distinct caller queue is active.
#[path = "support/managed_pcm.rs"]
mod managed_pcm;

#[test]
fn stock_pcm_closes_and_drops_from_active_caller_queue_and_replays() {
    managed_pcm::check_queued_teardown_replay("PULSEBEAM_PCM_TEARDOWN_TRACE_PATH");
}
