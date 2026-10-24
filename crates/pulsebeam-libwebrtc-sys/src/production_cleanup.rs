//! Final-owner cleanup cannot synchronously destroy its current SDK role.
use super::*;
use std::ops::Deref;

pub(super) struct CaptiveFactory(Option<PeerConnectionFactory>);
impl CaptiveFactory {
    pub(super) fn new(factory: PeerConnectionFactory) -> Self {
        Self(Some(factory))
    }
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_none()
    }
    fn take(&mut self) -> Self {
        Self(self.0.take())
    }
}
impl Deref for CaptiveFactory {
    type Target = PeerConnectionFactory;
    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .expect("factory is moved only during final teardown")
    }
}
impl<R: SessionRuntime> Session<R> {
    pub(super) fn take_for_cleanup(&mut self) -> Self {
        // Move every Rc-containing field, not clones. Construction requires all
        // fields, so future resources cannot silently be omitted from handoff.
        Self {
            local_stream_observers: std::mem::take(&mut self.local_stream_observers),
            video_encoders: std::mem::take(&mut self.video_encoders),
            video_decoders: std::mem::take(&mut self.video_decoders),
            encoded_images: std::mem::take(&mut self.encoded_images),
            encoded_buffers: std::mem::take(&mut self.encoded_buffers),
            encoded_writers: std::mem::take(&mut self.encoded_writers),
            encoded_callbacks: std::mem::take(&mut self.encoded_callbacks),
            decoded_callbacks: std::mem::take(&mut self.decoded_callbacks),
            fec_overrides: std::mem::take(&mut self.fec_overrides),
            video_selectors: std::mem::take(&mut self.video_selectors),
            sender_parameter_operations: std::mem::take(&mut self.sender_parameter_operations),
            video_buffer_preparations: std::mem::take(&mut self.video_buffer_preparations),
            raw_video_frames: std::mem::take(&mut self.raw_video_frames),
            raw_video_buffers: std::mem::take(&mut self.raw_video_buffers),
            writable_i420_buffers: std::mem::take(&mut self.writable_i420_buffers),
            video_source_encoded_sinks: std::mem::take(&mut self.video_source_encoded_sinks),
            video_observers: std::mem::take(&mut self.video_observers),
            video_source_views: std::mem::take(&mut self.video_source_views),
            streams: std::mem::take(&mut self.streams),
            stream_tracks: std::mem::take(&mut self.stream_tracks),
            audio_track_processors: std::mem::take(&mut self.audio_track_processors),
            audio_observers: std::mem::take(&mut self.audio_observers),
            audio_source_views: std::mem::take(&mut self.audio_source_views),
            frame_transformers: std::mem::take(&mut self.frame_transformers),
            media_sinks: std::mem::take(&mut self.media_sinks),
            connectivity: std::mem::take(&mut self.connectivity),
            sframe_encrypters: std::mem::take(&mut self.sframe_encrypters),
            sframe_decrypters: std::mem::take(&mut self.sframe_decrypters),
            frame_encryptors: std::mem::take(&mut self.frame_encryptors),
            frame_decryptors: std::mem::take(&mut self.frame_decryptors),
            sender_observers: std::mem::take(&mut self.sender_observers),
            receiver_observers: std::mem::take(&mut self.receiver_observers),
            channels: std::mem::take(&mut self.channels),
            video_sinks: std::mem::take(&mut self.video_sinks),
            track_observers: std::mem::take(&mut self.track_observers),
            stream_observers: std::mem::take(&mut self.stream_observers),
            media_streams: std::mem::take(&mut self.media_streams),
            track_sources: std::mem::take(&mut self.track_sources),
            audio_processors: std::mem::take(&mut self.audio_processors),
            media_tracks: std::mem::take(&mut self.media_tracks),
            transceivers: std::mem::take(&mut self.transceivers),
            senders: std::mem::take(&mut self.senders),
            receivers: std::mem::take(&mut self.receivers),
            sources: std::mem::take(&mut self.sources),
            #[cfg(feature = "native")]
            desktop_options: std::mem::take(&mut self.desktop_options),
            #[cfg(feature = "native")]
            desktop_metadata: std::mem::take(&mut self.desktop_metadata),
            #[cfg(feature = "native")]
            camera_options: std::mem::take(&mut self.camera_options),
            #[cfg(feature = "native")]
            camera_device_info: std::mem::take(&mut self.camera_device_info),
            video_bitrate_allocator_factory: self.video_bitrate_allocator_factory.take(),
            retirement: self.retirement.take(),
            runtime: self.runtime.take(),
            peers: std::mem::take(&mut self.peers),
            input: self.input.take(),
            factory: self.factory.take(),
            readiness: self.readiness.clone(),
            closed: self.closed,
            _exclusive: PhantomData,
        }
    }
}
