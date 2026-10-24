//! Captive media references and explicit notifiers for the production actor.
use super::*;

impl<R: SessionRuntime> Session<R> {
    pub fn senders(&mut self, peer: SessionPeerId) -> Result<Vec<SessionSenderId>, PeerError> {
        self.check_open()?;
        let values = self.peers.get(&peer).ok_or_else(missing)?.value.senders();
        values
            .into_iter()
            .map(|v| self.insert_sender(peer, v))
            .collect()
    }
    pub fn receivers(&mut self, peer: SessionPeerId) -> Result<Vec<SessionReceiverId>, PeerError> {
        self.check_open()?;
        let values = self.peers.get(&peer).ok_or_else(missing)?.value.receivers();
        values
            .into_iter()
            .map(|v| self.insert_receiver(peer, v))
            .collect()
    }
    pub fn transceivers(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<Vec<SessionTransceiverId>, PeerError> {
        self.check_open()?;
        let values = self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .transceivers()?;
        values
            .into_iter()
            .map(|v| self.insert_transceiver(peer, v))
            .collect()
    }
    /// Actual negotiated SDK MID for this transceiver. It remains absent until
    /// native description application associates the transceiver with an m-line.
    pub fn transceiver_mid(
        &mut self,
        transceiver: SessionTransceiverId,
    ) -> Result<Option<String>, PeerError> {
        self.check_open()?;
        Ok(self
            .transceivers
            .get(&transceiver)
            .ok_or_else(missing)?
            .value
            .value
            .mid())
    }

    pub fn create_local_media_stream(
        &mut self,
        peer: SessionPeerId,
        id: &[u8],
    ) -> Result<SessionMediaStreamId, PeerError> {
        self.check_open()?;
        self.peers.get(&peer).ok_or_else(missing)?;
        let stream = self.factory.create_local_media_stream(id)?;
        self.insert_media_stream(peer, stream)
    }
    pub fn add_media_stream_track(
        &mut self,
        stream: SessionMediaStreamId,
        track: SessionMediaTrackId,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        self.media_streams
            .get(&stream)
            .ok_or_else(missing)?
            .value
            .add_track(&self.media_tracks.get(&track).ok_or_else(missing)?.value)
    }
    pub fn remove_media_stream_track(
        &mut self,
        stream: SessionMediaStreamId,
        track: SessionMediaTrackId,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        Ok(self
            .media_streams
            .get(&stream)
            .ok_or_else(missing)?
            .value
            .remove_track(&self.media_tracks.get(&track).ok_or_else(missing)?.value))
    }
    pub fn add_stream(
        &mut self,
        peer: SessionPeerId,
        stream: SessionMediaStreamId,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .add_stream(&self.media_streams.get(&stream).ok_or_else(missing)?.value)
    }
    pub fn remove_stream(
        &mut self,
        peer: SessionPeerId,
        stream: SessionMediaStreamId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .remove_stream(&self.media_streams.get(&stream).ok_or_else(missing)?.value)
    }
    fn peer_streams(
        &mut self,
        peer: SessionPeerId,
        remote: bool,
    ) -> Result<Vec<SessionMediaStreamId>, PeerError> {
        self.check_open()?;
        let peer_value = &self.peers.get(&peer).ok_or_else(missing)?.value;
        let streams = if remote {
            peer_value.remote_streams()?
        } else {
            peer_value.local_streams()?
        };
        streams
            .into_iter()
            .map(|value| self.insert_media_stream(peer, value))
            .collect()
    }
    pub fn local_streams(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<Vec<SessionMediaStreamId>, PeerError> {
        self.peer_streams(peer, false)
    }
    pub fn remote_streams(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<Vec<SessionMediaStreamId>, PeerError> {
        self.peer_streams(peer, true)
    }
    pub(super) fn insert_media_stream(
        &mut self,
        peer: SessionPeerId,
        value: crate::MediaStream,
    ) -> Result<SessionMediaStreamId, PeerError> {
        if let Some((&id, _)) = self
            .media_streams
            .iter()
            .find(|(_, r)| r.value.same_resource(&value))
        {
            return Ok(id);
        }
        let id = SessionMediaStreamId(next_id()?);
        self.media_streams.insert(id, Resource { peer, value });
        Ok(id)
    }
    pub fn set_media_track_volume(
        &mut self,
        track: SessionMediaTrackId,
        volume: f64,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .set_audio_volume(volume)
    }
    pub fn set_media_track_should_receive(
        &mut self,
        track: SessionMediaTrackId,
        receive: bool,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .set_should_receive(receive)
    }
    pub fn set_media_track_content_hint(
        &mut self,
        track: SessionMediaTrackId,
        hint: crate::VideoContentHint,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .set_content_hint(hint)
    }
    pub fn set_media_track_audio_options(
        &mut self,
        track: SessionMediaTrackId,
        options: &crate::AudioSourceOptions,
    ) -> Result<crate::TrackAudioOptionsResult, PeerError> {
        self.check_open()?;
        self.media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .set_audio_options(options)
    }

    pub fn set_sender_stream_id_bytes(
        &mut self,
        sender: SessionSenderId,
        ids: &[Vec<u8>],
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_stream_id_bytes(ids);
        Ok(())
    }
    pub fn receiver_native_id(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Vec<u8>, PeerError> {
        Ok(self
            .receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .id_bytes())
    }
    pub fn sender_native_id_bytes(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Vec<u8>, PeerError> {
        Ok(self
            .senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .id_bytes())
    }
    pub fn receiver_media_type(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<crate::MediaType, PeerError> {
        Ok(self
            .receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .media_type())
    }
    pub fn receiver_stream_ids(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Vec<Vec<u8>>, PeerError> {
        Ok(self
            .receivers
            .get(&receiver)
            .ok_or_else(missing)?
            .value
            .stream_id_bytes())
    }
    pub fn receiver_streams(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Vec<SessionMediaStreamId>, PeerError> {
        let r = self.receivers.get(&receiver).ok_or_else(missing)?;
        let peer = r.peer;
        let streams = r.value.streams();
        streams
            .into_iter()
            .map(|value| self.insert_media_stream(peer, value))
            .collect()
    }
    pub fn media_stream_id(&mut self, stream: SessionMediaStreamId) -> Result<Vec<u8>, PeerError> {
        Ok(self
            .media_streams
            .get(&stream)
            .ok_or_else(missing)?
            .value
            .id())
    }
    fn stream_tracks(
        &mut self,
        stream: SessionMediaStreamId,
        video: bool,
    ) -> Result<Vec<SessionMediaTrackId>, PeerError> {
        let r = self.media_streams.get(&stream).ok_or_else(missing)?;
        let peer = r.peer;
        let tracks = if video {
            r.value.video_tracks()
        } else {
            r.value.audio_tracks()
        };
        tracks
            .into_iter()
            .map(|track| self.insert_media_track(peer, track))
            .collect()
    }
    pub fn media_stream_audio_tracks(
        &mut self,
        stream: SessionMediaStreamId,
    ) -> Result<Vec<SessionMediaTrackId>, PeerError> {
        self.stream_tracks(stream, false)
    }
    pub fn media_stream_video_tracks(
        &mut self,
        stream: SessionMediaStreamId,
    ) -> Result<Vec<SessionMediaTrackId>, PeerError> {
        self.stream_tracks(stream, true)
    }
    fn stream_find_track(
        &mut self,
        stream: SessionMediaStreamId,
        video: bool,
        id: &[u8],
    ) -> Result<Option<SessionMediaTrackId>, PeerError> {
        let r = self.media_streams.get(&stream).ok_or_else(missing)?;
        let peer = r.peer;
        let track = if video {
            r.value.find_video_track(id)
        } else {
            r.value.find_audio_track(id)
        };
        track.map(|t| self.insert_media_track(peer, t)).transpose()
    }
    pub fn media_stream_find_audio_track(
        &mut self,
        stream: SessionMediaStreamId,
        id: &[u8],
    ) -> Result<Option<SessionMediaTrackId>, PeerError> {
        self.stream_find_track(stream, false, id)
    }
    pub fn media_stream_find_video_track(
        &mut self,
        stream: SessionMediaStreamId,
        id: &[u8],
    ) -> Result<Option<SessionMediaTrackId>, PeerError> {
        self.stream_find_track(stream, true, id)
    }
    pub fn release_media_stream(&mut self, stream: SessionMediaStreamId) -> Result<(), PeerError> {
        self.stream_observers.remove(&stream);
        self.media_streams.remove(&stream).ok_or_else(missing)?;
        Ok(())
    }
    pub fn media_track_source(
        &mut self,
        track: SessionMediaTrackId,
    ) -> Result<Option<SessionTrackSourceId>, PeerError> {
        let r = self.media_tracks.get(&track).ok_or_else(missing)?;
        let peer = r.peer;
        let Some(value) = r.value.source() else {
            return Ok(None);
        };
        if let Some((&id, _)) = self
            .track_sources
            .iter()
            .find(|(_, r)| r.value.same_resource(&value))
        {
            return Ok(Some(id));
        }
        let id = SessionTrackSourceId(next_id()?);
        self.track_sources.insert(id, Resource { peer, value });
        Ok(Some(id))
    }
    pub fn release_track_source(&mut self, source: SessionTrackSourceId) -> Result<(), PeerError> {
        self.track_sources.remove(&source).ok_or_else(missing)?;
        Ok(())
    }
    pub fn track_source_kind(
        &mut self,
        source: SessionTrackSourceId,
    ) -> Result<crate::MediaTrackKind, PeerError> {
        Ok(self
            .track_sources
            .get(&source)
            .ok_or_else(missing)?
            .value
            .kind())
    }
    pub fn track_source_state(
        &mut self,
        source: SessionTrackSourceId,
    ) -> Result<crate::MediaSourceState, PeerError> {
        Ok(self
            .track_sources
            .get(&source)
            .ok_or_else(missing)?
            .value
            .state())
    }
    pub fn track_source_remote(&mut self, source: SessionTrackSourceId) -> Result<bool, PeerError> {
        Ok(self
            .track_sources
            .get(&source)
            .ok_or_else(missing)?
            .value
            .remote())
    }
    pub fn track_source_audio_options(
        &mut self,
        source: SessionTrackSourceId,
    ) -> Result<Option<crate::AudioSourceOptions>, PeerError> {
        Ok(
            match &self.track_sources.get(&source).ok_or_else(missing)?.value {
                crate::MediaTrackSource::Audio(x) => Some(x.options()),
                _ => None,
            },
        )
    }
    pub fn track_source_video_info(
        &mut self,
        source: SessionTrackSourceId,
    ) -> Result<Option<crate::VideoSourceInfo>, PeerError> {
        Ok(
            match &self.track_sources.get(&source).ok_or_else(missing)?.value {
                crate::MediaTrackSource::Video(x) => Some(x.info()),
                _ => None,
            },
        )
    }
    pub fn media_track_audio_processor(
        &mut self,
        track: SessionMediaTrackId,
    ) -> Result<Option<SessionAudioProcessorId>, PeerError> {
        let r = self.media_tracks.get(&track).ok_or_else(missing)?;
        let peer = r.peer;
        let Some(value) = r.value.audio_processor() else {
            return Ok(None);
        };
        if let Some((&id, _)) = self
            .audio_processors
            .iter()
            .find(|(_, r)| r.value.same_resource(&value))
        {
            return Ok(Some(id));
        }
        let id = SessionAudioProcessorId(next_id()?);
        self.audio_processors.insert(id, Resource { peer, value });
        Ok(Some(id))
    }
    pub fn audio_processor_stats(
        &mut self,
        processor: SessionAudioProcessorId,
        has_remote_tracks: bool,
    ) -> Result<crate::AudioProcessorStats, PeerError> {
        Ok(self
            .audio_processors
            .get(&processor)
            .ok_or_else(missing)?
            .value
            .stats(has_remote_tracks))
    }
    pub fn release_audio_processor(
        &mut self,
        processor: SessionAudioProcessorId,
    ) -> Result<(), PeerError> {
        self.audio_processors
            .remove(&processor)
            .ok_or_else(missing)?;
        Ok(())
    }
    pub fn media_track_audio_volume(
        &mut self,
        track: SessionMediaTrackId,
    ) -> Result<Option<f64>, PeerError> {
        Ok(self
            .media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .audio_volume())
    }
    pub fn media_track_signal_level(
        &mut self,
        track: SessionMediaTrackId,
    ) -> Result<Option<i32>, PeerError> {
        Ok(self
            .media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .signal_level())
    }
    pub fn media_track_should_receive(
        &mut self,
        track: SessionMediaTrackId,
    ) -> Result<Option<bool>, PeerError> {
        Ok(self
            .media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .should_receive())
    }
    pub fn media_track_content_hint(
        &mut self,
        track: SessionMediaTrackId,
    ) -> Result<Option<crate::VideoContentHint>, PeerError> {
        Ok(self
            .media_tracks
            .get(&track)
            .ok_or_else(missing)?
            .value
            .content_hint())
    }
    /// Register one SDK slot. A repeated registration through a canonical alias
    /// leaves the existing capacity and subscription intact.
    pub fn register_media_track_observer(
        &mut self,
        id: SessionMediaTrackId,
        capacity: std::num::NonZeroUsize,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        let r = self.media_tracks.get(&id).ok_or_else(missing)?;
        if self.track_observers.contains_key(&id) {
            return Ok(false);
        }
        self.track_observers.insert(
            id,
            Resource {
                peer: r.peer,
                value: r.value.observe(capacity),
            },
        );
        Ok(true)
    }
    pub fn unregister_media_track_observer(
        &mut self,
        id: SessionMediaTrackId,
    ) -> Result<(), PeerError> {
        self.media_tracks.get(&id).ok_or_else(missing)?;
        self.track_observers.remove(&id);
        Ok(())
    }
    pub fn take_media_track_event(
        &mut self,
        id: SessionMediaTrackId,
    ) -> Result<Option<crate::MediaChange>, PeerError> {
        self.media_tracks.get(&id).ok_or_else(missing)?;
        Ok(self
            .track_observers
            .get(&id)
            .and_then(|o| o.value.take_event()))
    }
    pub fn take_media_track_dropped_events(
        &mut self,
        id: SessionMediaTrackId,
    ) -> Result<u64, PeerError> {
        self.media_tracks.get(&id).ok_or_else(missing)?;
        Ok(self
            .track_observers
            .get(&id)
            .map_or(0, |o| o.value.take_dropped_events()))
    }
    /// Register one SDK slot. A repeated registration through a canonical alias
    /// leaves the existing capacity and subscription intact.
    pub fn register_media_stream_observer(
        &mut self,
        id: SessionMediaStreamId,
        capacity: std::num::NonZeroUsize,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        let r = self.media_streams.get(&id).ok_or_else(missing)?;
        if self.stream_observers.contains_key(&id) {
            return Ok(false);
        }
        self.stream_observers.insert(
            id,
            Resource {
                peer: r.peer,
                value: r.value.observe(capacity),
            },
        );
        Ok(true)
    }
    pub fn unregister_media_stream_observer(
        &mut self,
        id: SessionMediaStreamId,
    ) -> Result<(), PeerError> {
        self.media_streams.get(&id).ok_or_else(missing)?;
        self.stream_observers.remove(&id);
        Ok(())
    }
    pub fn take_media_stream_event(
        &mut self,
        id: SessionMediaStreamId,
    ) -> Result<Option<crate::MediaChange>, PeerError> {
        self.media_streams.get(&id).ok_or_else(missing)?;
        Ok(self
            .stream_observers
            .get(&id)
            .and_then(|o| o.value.take_event()))
    }
    pub fn take_media_stream_dropped_events(
        &mut self,
        id: SessionMediaStreamId,
    ) -> Result<u64, PeerError> {
        self.media_streams.get(&id).ok_or_else(missing)?;
        Ok(self
            .stream_observers
            .get(&id)
            .map_or(0, |o| o.value.take_dropped_events()))
    }
}
