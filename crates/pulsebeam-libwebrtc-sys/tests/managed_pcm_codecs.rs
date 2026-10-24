//! Advertised stock audio families use the same PCM/native transport application.
//! Plaintext encoded bytes, decoded PCM, native reports and external loss
//! decisions participate in the repeated caller-driven replay comparison.
#[path = "support/managed_pcm.rs"]
mod managed_pcm;

#[test]
fn stock_pcm_pcmu_transport_and_reports_replay() {
    managed_pcm::check_codec_sustained_replay("PCMU", 8000, false, "PULSEBEAM_PCM_PCMU_TRACE_PATH");
}

#[test]
fn stock_pcm_pcmu_with_external_rtp_loss_replays() {
    managed_pcm::check_codec_sustained_replay(
        "PCMU",
        8000,
        true,
        "PULSEBEAM_PCM_PCMU_LOSS_TRACE_PATH",
    );
}

#[test]
fn stock_pcm_pcma_transport_and_reports_replay() {
    managed_pcm::check_codec_sustained_replay("PCMA", 8000, false, "PULSEBEAM_PCM_PCMA_TRACE_PATH");
}

#[test]
fn stock_pcm_pcma_with_external_rtp_loss_replays() {
    managed_pcm::check_codec_sustained_replay(
        "PCMA",
        8000,
        true,
        "PULSEBEAM_PCM_PCMA_LOSS_TRACE_PATH",
    );
}

#[test]
fn stock_pcm_g722_transport_and_reports_replay() {
    managed_pcm::check_codec_sustained_replay(
        "G722",
        16000,
        false,
        "PULSEBEAM_PCM_G722_TRACE_PATH",
    );
}

#[test]
fn stock_pcm_g722_with_external_rtp_loss_replays() {
    managed_pcm::check_codec_sustained_replay(
        "G722",
        16000,
        true,
        "PULSEBEAM_PCM_G722_LOSS_TRACE_PATH",
    );
}
