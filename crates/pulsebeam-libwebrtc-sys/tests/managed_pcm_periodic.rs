//! Sustained stock audio, native RTCP reports and externally chosen loss.
#[path = "support/managed_pcm.rs"]
mod managed_pcm;

#[test]
fn sustained_stock_pcm_delivers_native_reports_and_replays() {
    managed_pcm::check_sustained_replay(false, "PULSEBEAM_PCM_PERIODIC_TRACE_PATH");
}

#[test]
fn sustained_stock_pcm_with_explicit_drops_reports_native_loss_and_replays() {
    managed_pcm::check_sustained_replay(true, "PULSEBEAM_PCM_PERIODIC_DROP_TRACE_PATH");
}
