//! A shared-library consumer that retains real RTC and software media code.
use pulsebeam_webrtc_sys::{
    PeerConfiguration, PeerError, ProductionSession, RtpTransceiverDirection, SessionConfig,
};

fn construct_and_retire() -> Result<(), PeerError> {
    let mut session = ProductionSession::new(SessionConfig::default())?;
    let peer = session.create_peer(PeerConfiguration::default())?;
    let audio = session.create_pcm_source()?;
    let video = session.create_raw_video_source()?;
    session.publish_pcm(
        peer,
        audio,
        "audio",
        RtpTransceiverDirection::SendOnly,
        None,
    )?;
    session.publish_video(peer, video, "video", RtpTransceiverDirection::SendOnly, &[])?;
    session.create_offer(peer, false)?;
    session.shutdown()
}

/// Return a C status code instead of unwinding across the consumer boundary.
#[unsafe(no_mangle)]
pub extern "C" fn pulsebeam_dynamic_smoke() -> i32 {
    match construct_and_retire() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("dynamic RTC consumer failed: {error}");
            1
        }
    }
}
