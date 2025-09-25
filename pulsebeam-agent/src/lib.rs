#[cfg(all(feature = "ffmpeg-static", feature = "ffmpeg-dynamic"))]
compile_error!("Cannot enable both ffmpeg-static and ffmpeg-dynamic features simultaneously");

#[cfg(not(any(feature = "ffmpeg-static", feature = "ffmpeg-dynamic")))]
compile_error!("Must enable either ffmpeg-static or ffmpeg-dynamic feature");

pub mod agent;
// pub mod rt;
