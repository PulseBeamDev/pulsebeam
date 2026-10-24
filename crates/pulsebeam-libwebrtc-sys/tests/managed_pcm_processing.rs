//! Explicit builtin APM processing through the same real PCM Calls.
#[path = "support/managed_pcm.rs"]
mod managed_pcm;

use pulsebeam_webrtc_sys::AudioProcessorConfig;

#[test]
fn stock_pcm_builtin_preamp_uses_managed_worker_and_replays() {
    let mut config = AudioProcessorConfig::default();
    config.pre_amplifier.enabled = true;
    config.pre_amplifier.fixed_gain_factor = 2.0;
    managed_pcm::check_replay(Some(&config), "PULSEBEAM_PCM_PROCESSING_TRACE_PATH");
}
