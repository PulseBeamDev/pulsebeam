//! Stock PCM -> Opus -> NetEq with caller-owned execution.
#[path = "support/managed_pcm.rs"]
mod managed_pcm;

#[test]
fn stock_pcm_opus_uses_managed_call_queues_and_replays() {
    managed_pcm::check_replay(None, "PULSEBEAM_PCM_TRACE_PATH");
}
