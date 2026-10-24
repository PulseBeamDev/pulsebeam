//! Captive codec resources. Native products marshal every operation and final
//! destruction to the retained factory worker, never the migrating caller.
use super::*;
use crate::*;

impl<R: SessionRuntime> Session<R> {
    pub fn video_encoder_formats(&mut self) -> Result<Vec<SdpVideoFormat>, PeerError> {
        self.check_open()?;
        self.factory
            .codec_encoder_factory()
            .ok_or_else(missing)?
            .try_supported_format_values()
            .map_err(|e| error(PeerErrorKind::Internal, &e.to_string()))
    }
    pub fn video_encoder_implementations(&mut self) -> Result<Vec<SdpVideoFormat>, PeerError> {
        self.check_open()?;
        self.factory
            .codec_encoder_factory()
            .ok_or_else(missing)?
            .try_implementation_values()
            .map_err(|e| error(PeerErrorKind::Internal, &e.to_string()))
    }
    pub fn video_decoder_formats(&mut self) -> Result<Vec<SdpVideoFormat>, PeerError> {
        self.check_open()?;
        self.factory
            .codec_decoder_factory()
            .ok_or_else(missing)?
            .try_supported_format_values()
            .map_err(|e| error(PeerErrorKind::Internal, &e.to_string()))
    }
    pub fn video_encoder_query(
        &mut self,
        format: &SdpVideoFormat,
        mode: Option<&[u8]>,
        resolution: Option<&Resolution>,
    ) -> Result<CodecSupport, PeerError> {
        self.check_open()?;
        self.factory
            .codec_encoder_factory()
            .ok_or_else(missing)?
            .query_support_value(format, mode, resolution)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn video_encoder_query_legacy(
        &mut self,
        format: &SdpVideoFormat,
        mode: Option<&[u8]>,
    ) -> Result<CodecSupport, PeerError> {
        self.check_open()?;
        self.factory
            .codec_encoder_factory()
            .ok_or_else(missing)?
            .query_support_legacy_value(format, mode)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn video_decoder_query_legacy(
        &mut self,
        format: &SdpVideoFormat,
        reference_scaling: bool,
    ) -> Result<CodecSupport, PeerError> {
        self.check_open()?;
        self.factory
            .codec_decoder_factory()
            .ok_or_else(missing)?
            .query_support_legacy_value(format, reference_scaling)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn video_decoder_query(
        &mut self,
        format: &SdpVideoFormat,
        reference_scaling: bool,
        resolution: Option<&Resolution>,
    ) -> Result<CodecSupport, PeerError> {
        self.check_open()?;
        self.factory
            .codec_decoder_factory()
            .ok_or_else(missing)?
            .query_support_value(format, reference_scaling, resolution)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn create_video_encoder(
        &mut self,
        format: &SdpVideoFormat,
    ) -> Result<SessionVideoEncoderId, PeerError> {
        self.check_open()?;
        let id = SessionVideoEncoderId(next_id()?);
        let value = OwnedVideoEncoder::on_actor(&self.factory, format)
            .map_err(|e| error(PeerErrorKind::NativeConstruction, &e.to_string()))?;
        self.video_encoders.insert(id, value);
        Ok(id)
    }
    pub fn create_video_decoder(
        &mut self,
        format: &SdpVideoFormat,
    ) -> Result<SessionVideoDecoderId, PeerError> {
        self.check_open()?;
        let id = SessionVideoDecoderId(next_id()?);
        let value = OwnedVideoDecoder::on_actor(&self.factory, format)
            .map_err(|e| error(PeerErrorKind::NativeConstruction, &e.to_string()))?;
        self.video_decoders.insert(id, value);
        Ok(id)
    }
    pub fn initialize_video_encoder(
        &mut self,
        encoder: SessionVideoEncoderId,
        codec: &OwnedVideoCodec,
        settings: VideoEncoderSettingsValue,
    ) -> Result<i32, PeerError> {
        self.check_open()?;
        Ok(self
            .video_encoders
            .get_mut(&encoder)
            .ok_or_else(missing)?
            .initialize(codec, settings))
    }
    pub fn initialize_video_encoder_legacy(
        &mut self,
        encoder: SessionVideoEncoderId,
        codec: &OwnedVideoCodec,
        cores: i32,
        payload: usize,
    ) -> Result<i32, PeerError> {
        self.check_open()?;
        Ok(self
            .video_encoders
            .get_mut(&encoder)
            .ok_or_else(missing)?
            .initialize_legacy(codec, cores, payload))
    }
    pub fn encode_video_frame(
        &mut self,
        encoder: SessionVideoEncoderId,
        frame: SessionRawVideoFrameId,
        types: Option<&[EncodedImageFrameType]>,
    ) -> Result<i32, PeerError> {
        self.check_open()?;
        let frame = self.raw_video_frames.get(&frame).ok_or_else(missing)?;
        Ok(self
            .video_encoders
            .get_mut(&encoder)
            .ok_or_else(missing)?
            .encode(frame, types))
    }
    pub fn configure_video_decoder(
        &mut self,
        decoder: SessionVideoDecoderId,
        settings: &OwnedVideoDecoderSettings,
    ) -> Result<bool, PeerError> {
        self.check_open()?;
        Ok(self
            .video_decoders
            .get_mut(&decoder)
            .ok_or_else(missing)?
            .configure(settings))
    }
    pub fn decode_video_image(
        &mut self,
        decoder: SessionVideoDecoderId,
        image: SessionEncodedImageId,
        render_time_ms: i64,
    ) -> Result<i32, PeerError> {
        self.check_open()?;
        let image = self.encoded_images.get(&image).ok_or_else(missing)?;
        self.video_decoders
            .get_mut(&decoder)
            .ok_or_else(missing)?
            .decode(image, render_time_ms)
            .map_err(codec_error)
    }
    pub fn decode_video_image_with_missing_frames(
        &mut self,
        decoder: SessionVideoDecoderId,
        image: SessionEncodedImageId,
        missing_frames: bool,
        render_time_ms: i64,
    ) -> Result<i32, PeerError> {
        self.check_open()?;
        let image = self.encoded_images.get(&image).ok_or_else(missing)?;
        self.video_decoders
            .get_mut(&decoder)
            .ok_or_else(missing)?
            .decode_with_missing_frames(image, missing_frames, render_time_ms)
            .map_err(codec_error)
    }
    pub fn set_video_encoder_rates(
        &mut self,
        encoder: SessionVideoEncoderId,
        rates: &OwnedVideoRateControl,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_encoders
            .get_mut(&encoder)
            .ok_or_else(missing)?
            .set_rates(rates);
        Ok(())
    }
    pub fn video_encoder_packet_loss(
        &mut self,
        encoder: SessionVideoEncoderId,
        rate: f32,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_encoders
            .get_mut(&encoder)
            .ok_or_else(missing)?
            .on_packet_loss_rate_update(rate);
        Ok(())
    }
    pub fn video_encoder_rtt(
        &mut self,
        encoder: SessionVideoEncoderId,
        milliseconds: i64,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_encoders
            .get_mut(&encoder)
            .ok_or_else(missing)?
            .on_rtt_update(milliseconds);
        Ok(())
    }
    pub fn video_encoder_loss_notification(
        &mut self,
        encoder: SessionVideoEncoderId,
        notification: VideoEncoderLossNotification,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_encoders
            .get_mut(&encoder)
            .ok_or_else(missing)?
            .on_loss_notification(notification);
        Ok(())
    }

    pub fn video_encoder_info(
        &mut self,
        id: SessionVideoEncoderId,
    ) -> Result<OwnedVideoEncoderInfo, PeerError> {
        self.check_open()?;
        self.video_encoders
            .get(&id)
            .ok_or_else(missing)?
            .info()
            .map_err(codec_error)
    }
    pub fn video_encoder_name(&mut self, id: SessionVideoEncoderId) -> Result<Vec<u8>, PeerError> {
        self.check_open()?;
        self.video_encoders
            .get(&id)
            .ok_or_else(missing)?
            .implementation_name_bytes()
            .map_err(codec_error)
    }
    pub fn video_encoder_failure_observer(
        &mut self,
        id: SessionVideoEncoderId,
    ) -> Result<Option<VideoFactoryFailureObserver>, PeerError> {
        self.check_open()?;
        Ok(self
            .video_encoders
            .get(&id)
            .ok_or_else(missing)?
            .failure_observer())
    }
    pub fn release_video_encoder(&mut self, id: SessionVideoEncoderId) -> Result<i32, PeerError> {
        self.check_open()?;
        Ok(self
            .video_encoders
            .get_mut(&id)
            .ok_or_else(missing)?
            .release())
    }
    pub fn destroy_video_encoder(&mut self, id: SessionVideoEncoderId) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_encoders.remove(&id).ok_or_else(missing)?;
        Ok(())
    }

    pub fn video_decoder_info(
        &mut self,
        id: SessionVideoDecoderId,
    ) -> Result<OwnedVideoDecoderInfo, PeerError> {
        self.check_open()?;
        self.video_decoders
            .get(&id)
            .ok_or_else(missing)?
            .info()
            .map_err(codec_error)
    }
    pub fn video_decoder_name(&mut self, id: SessionVideoDecoderId) -> Result<Vec<u8>, PeerError> {
        self.check_open()?;
        self.video_decoders
            .get(&id)
            .ok_or_else(missing)?
            .implementation_name_bytes()
            .map_err(codec_error)
    }
    pub fn video_decoder_failure_observer(
        &mut self,
        id: SessionVideoDecoderId,
    ) -> Result<Option<VideoFactoryFailureObserver>, PeerError> {
        self.check_open()?;
        Ok(self
            .video_decoders
            .get(&id)
            .ok_or_else(missing)?
            .failure_observer())
    }
    pub fn release_video_decoder(&mut self, id: SessionVideoDecoderId) -> Result<i32, PeerError> {
        self.check_open()?;
        Ok(self
            .video_decoders
            .get_mut(&id)
            .ok_or_else(missing)?
            .release())
    }
    pub fn destroy_video_decoder(&mut self, id: SessionVideoDecoderId) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_decoders.remove(&id).ok_or_else(missing)?;
        Ok(())
    }

    pub fn create_encoded_image_callback(
        &mut self,
        provider: Arc<dyn EncodedImageCallbackProvider>,
    ) -> Result<SessionEncodedImageCallbackId, PeerError> {
        self.check_open()?;
        let id = SessionEncodedImageCallbackId(next_id()?);
        self.encoded_callbacks
            .insert(id, EncodedImageCallbackSink::from_shared(provider));
        Ok(id)
    }
    pub fn create_decoded_image_callback(
        &mut self,
        provider: Arc<dyn DecodedImageCallbackProvider>,
    ) -> Result<SessionDecodedImageCallbackId, PeerError> {
        self.check_open()?;
        let id = SessionDecodedImageCallbackId(next_id()?);
        self.decoded_callbacks
            .insert(id, DecodedImageCallbackSink::from_shared(provider));
        Ok(id)
    }
    pub fn register_video_encoder_callback(
        &mut self,
        encoder: SessionVideoEncoderId,
        callback: Option<SessionEncodedImageCallbackId>,
    ) -> Result<i32, PeerError> {
        self.check_open()?;
        let callback = callback
            .map(|id| self.encoded_callbacks.get(&id).ok_or_else(missing))
            .transpose()?;
        Ok(self
            .video_encoders
            .get_mut(&encoder)
            .ok_or_else(missing)?
            .register_callback(callback))
    }
    pub fn register_video_decoder_callback(
        &mut self,
        decoder: SessionVideoDecoderId,
        callback: Option<SessionDecodedImageCallbackId>,
    ) -> Result<i32, PeerError> {
        self.check_open()?;
        let callback = callback
            .map(|id| self.decoded_callbacks.get(&id).ok_or_else(missing))
            .transpose()?;
        Ok(self
            .video_decoders
            .get_mut(&decoder)
            .ok_or_else(missing)?
            .register_callback(callback))
    }
    pub fn create_fec_override(
        &mut self,
        provider: Arc<dyn FecControllerOverrideProvider>,
    ) -> Result<SessionFecOverrideId, PeerError> {
        self.check_open()?;
        let id = SessionFecOverrideId(next_id()?);
        self.fec_overrides
            .insert(id, FecControllerOverride::from_shared(provider));
        Ok(id)
    }
    pub fn set_video_encoder_fec_override(
        &mut self,
        encoder: SessionVideoEncoderId,
        callback: Option<SessionFecOverrideId>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let callback = callback
            .map(|id| self.fec_overrides.get(&id).ok_or_else(missing))
            .transpose()?;
        self.video_encoders
            .get_mut(&encoder)
            .ok_or_else(missing)?
            .set_fec_controller_override(callback);
        Ok(())
    }
    pub fn fec_override_allowed(
        &mut self,
        callback: SessionFecOverrideId,
        allowed: bool,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.fec_overrides
            .get(&callback)
            .ok_or_else(missing)?
            .set_fec_allowed(allowed)
            .map_err(codec_error)
    }
    pub fn invoke_encoded_image_callback(
        &mut self,
        callback: SessionEncodedImageCallbackId,
        image: SessionEncodedImageId,
        info: Option<&OwnedCodecSpecificInfo>,
    ) -> Result<EncodedImageCallbackResult, PeerError> {
        self.check_open()?;
        Ok(self
            .encoded_callbacks
            .get(&callback)
            .ok_or_else(missing)?
            .encoded(self.encoded_images.get(&image).ok_or_else(missing)?, info))
    }
    pub fn encoded_image_callback_frame_dropped(
        &mut self,
        callback: SessionEncodedImageCallbackId,
        timestamp: u32,
        spatial: i32,
        end: bool,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.encoded_callbacks
            .get(&callback)
            .ok_or_else(missing)?
            .frame_dropped(timestamp, spatial, end);
        Ok(())
    }
    pub fn invoke_decoded_image_callback(
        &mut self,
        callback: SessionDecodedImageCallbackId,
        frame: SessionRawVideoFrameId,
    ) -> Result<i32, PeerError> {
        self.check_open()?;
        Ok(self
            .decoded_callbacks
            .get(&callback)
            .ok_or_else(missing)?
            .decoded(self.raw_video_frames.get_mut(&frame).ok_or_else(missing)?))
    }
    pub fn invoke_decoded_image_callback_with_time(
        &mut self,
        callback: SessionDecodedImageCallbackId,
        frame: SessionRawVideoFrameId,
        time_ms: i64,
    ) -> Result<i32, PeerError> {
        self.check_open()?;
        Ok(self
            .decoded_callbacks
            .get(&callback)
            .ok_or_else(missing)?
            .decoded_with_time(
                self.raw_video_frames.get_mut(&frame).ok_or_else(missing)?,
                time_ms,
            ))
    }
    pub fn invoke_decoded_image_callback_with_metadata(
        &mut self,
        callback: SessionDecodedImageCallbackId,
        frame: SessionRawVideoFrameId,
        time_ms: Option<i32>,
        qp: Option<u8>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.decoded_callbacks
            .get(&callback)
            .ok_or_else(missing)?
            .decoded_with_metadata(
                self.raw_video_frames.get_mut(&frame).ok_or_else(missing)?,
                time_ms,
                qp,
            );
        Ok(())
    }
    pub fn fec_override_failure_observer(
        &mut self,
        callback: SessionFecOverrideId,
    ) -> Result<Option<VideoCodecCallbackFailureObserver>, PeerError> {
        self.check_open()?;
        Ok(self
            .fec_overrides
            .get(&callback)
            .ok_or_else(missing)?
            .failure_observer())
    }
    pub fn create_factory_video_encoder_selector(
        &mut self,
    ) -> Result<Option<SessionVideoEncoderSelectorId>, PeerError> {
        self.check_open()?;
        let value = self
            .factory
            .codec_encoder_factory()
            .ok_or_else(missing)?
            .encoder_selector()
            .map_err(|e| error(PeerErrorKind::Internal, &e.to_string()))?;
        value
            .map(|v| {
                let id = SessionVideoEncoderSelectorId(next_id()?);
                self.video_selectors.insert(id, v.into_shared());
                Ok(id)
            })
            .transpose()
    }
    pub fn create_video_encoder_selector(
        &mut self,
        provider: impl VideoEncoderSelectorProvider,
    ) -> Result<SessionVideoEncoderSelectorId, PeerError> {
        self.check_open()?;
        let id = SessionVideoEncoderSelectorId(next_id()?);
        self.video_selectors
            .insert(id, VideoEncoderSelector::from_provider(provider));
        Ok(id)
    }
    pub fn set_sender_owned_encoder_selector(
        &mut self,
        sender: SessionSenderId,
        selector: Option<OwnedVideoEncoderSelector>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_owned_encoder_selector(selector);
        Ok(())
    }
    pub fn set_sender_encoder_selector(
        &mut self,
        sender: SessionSenderId,
        selector: Option<SessionVideoEncoderSelectorId>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let value = selector
            .map(|id| self.video_selectors.get(&id).ok_or_else(missing))
            .transpose()?;
        self.senders
            .get(&sender)
            .ok_or_else(missing)?
            .value
            .set_encoder_selector(value);
        Ok(())
    }
    pub fn video_selector_current_encoder(
        &mut self,
        selector: SessionVideoEncoderSelectorId,
        format: &SdpVideoFormat,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_selectors
            .get(&selector)
            .ok_or_else(missing)?
            .on_current_encoder(format);
        Ok(())
    }
    pub fn video_selector_bitrate(
        &mut self,
        selector: SessionVideoEncoderSelectorId,
        rate: DataRate,
    ) -> Result<Option<SdpVideoFormat>, PeerError> {
        self.check_open()?;
        self.video_selectors
            .get(&selector)
            .ok_or_else(missing)?
            .on_available_bitrate(rate)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn video_selector_resolution(
        &mut self,
        selector: SessionVideoEncoderSelectorId,
        resolution: &RenderResolution,
    ) -> Result<Option<SdpVideoFormat>, PeerError> {
        self.check_open()?;
        Ok(self
            .video_selectors
            .get(&selector)
            .ok_or_else(missing)?
            .on_resolution_change(resolution))
    }
    pub fn video_selector_broken(
        &mut self,
        selector: SessionVideoEncoderSelectorId,
    ) -> Result<Option<SdpVideoFormat>, PeerError> {
        self.check_open()?;
        Ok(self
            .video_selectors
            .get(&selector)
            .ok_or_else(missing)?
            .on_encoder_broken())
    }
    pub fn video_selector_failure_observer(
        &mut self,
        selector: SessionVideoEncoderSelectorId,
    ) -> Result<VideoEncoderSelectorFailureObserver, PeerError> {
        self.check_open()?;
        Ok(self
            .video_selectors
            .get(&selector)
            .ok_or_else(missing)?
            .failure_observer())
    }
    pub fn create_encoded_image(&mut self) -> Result<SessionEncodedImageId, PeerError> {
        self.check_open()?;
        let id = SessionEncodedImageId(next_id()?);
        self.encoded_images.insert(id, EncodedImage::default());
        Ok(id)
    }
    pub fn encoded_image_metadata(
        &mut self,
        image: SessionEncodedImageId,
    ) -> Result<EncodedImageMetadata, PeerError> {
        self.check_open()?;
        Ok(self
            .encoded_images
            .get(&image)
            .ok_or_else(missing)?
            .metadata())
    }
    pub fn set_encoded_image_metadata(
        &mut self,
        image: SessionEncodedImageId,
        metadata: &EncodedImageMetadata,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.encoded_images
            .get_mut(&image)
            .ok_or_else(missing)?
            .set_metadata(metadata)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn encoded_image_bytes(
        &mut self,
        image: SessionEncodedImageId,
    ) -> Result<Vec<u8>, PeerError> {
        self.check_open()?;
        Ok(self
            .encoded_images
            .get(&image)
            .ok_or_else(missing)?
            .to_vec())
    }
    pub fn encoded_image_capture_time(
        &mut self,
        image: SessionEncodedImageId,
    ) -> Result<Timestamp, PeerError> {
        self.check_open()?;
        self.encoded_images
            .get(&image)
            .ok_or_else(missing)?
            .capture_time()
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn encoded_image_set_encode_time(
        &mut self,
        image: SessionEncodedImageId,
        start_ms: i64,
        finish_ms: i64,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.encoded_images
            .get_mut(&image)
            .ok_or_else(missing)?
            .set_encode_time(start_ms, finish_ms);
        Ok(())
    }
    pub fn encoded_image_len(&mut self, image: SessionEncodedImageId) -> Result<usize, PeerError> {
        self.check_open()?;
        Ok(self.encoded_images.get(&image).ok_or_else(missing)?.len())
    }
    pub fn encoded_image_set_len(
        &mut self,
        image: SessionEncodedImageId,
        length: usize,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.encoded_images
            .get_mut(&image)
            .ok_or_else(missing)?
            .set_len(length)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn encoded_image_set_buffer(
        &mut self,
        image: SessionEncodedImageId,
        buffer: Option<SessionEncodedVideoBufferId>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let buffer = buffer
            .map(|id| self.encoded_buffers.get(&id).ok_or_else(missing))
            .transpose()?;
        let image = self.encoded_images.get_mut(&image).ok_or_else(missing)?;
        if let Some(buffer) = buffer {
            image.set_buffer(buffer);
        } else {
            image.clear_buffer();
        }
        Ok(())
    }
    pub fn encoded_image_buffer(
        &mut self,
        image: SessionEncodedImageId,
    ) -> Result<Option<SessionEncodedVideoBufferId>, PeerError> {
        self.check_open()?;
        let value = self
            .encoded_images
            .get(&image)
            .ok_or_else(missing)?
            .buffer();
        value
            .map(|value| {
                let id = SessionEncodedVideoBufferId(next_id()?);
                self.encoded_buffers.insert(id, value);
                Ok(id)
            })
            .transpose()
    }
    pub fn encoded_image_layer_size(
        &mut self,
        image: SessionEncodedImageId,
        layer: i32,
    ) -> Result<Option<usize>, PeerError> {
        self.check_open()?;
        self.encoded_images
            .get(&image)
            .ok_or_else(missing)?
            .spatial_layer_frame_size(layer)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn encoded_image_set_layer_size(
        &mut self,
        image: SessionEncodedImageId,
        layer: i32,
        bytes: usize,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.encoded_images
            .get_mut(&image)
            .ok_or_else(missing)?
            .set_spatial_layer_frame_size(layer, bytes)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn create_encoded_video_buffer(
        &mut self,
        bytes: Vec<u8>,
    ) -> Result<SessionEncodedVideoBufferId, PeerError> {
        self.check_open()?;
        let id = SessionEncodedVideoBufferId(next_id()?);
        self.encoded_buffers
            .insert(id, EncodedVideoBuffer::from_owned_bytes(bytes));
        Ok(id)
    }
    pub fn encoded_video_buffer_bytes(
        &mut self,
        buffer: SessionEncodedVideoBufferId,
    ) -> Result<Vec<u8>, PeerError> {
        self.check_open()?;
        Ok(self
            .encoded_buffers
            .get(&buffer)
            .ok_or_else(missing)?
            .to_vec())
    }
    pub fn encoded_video_buffer_len(
        &mut self,
        buffer: SessionEncodedVideoBufferId,
    ) -> Result<usize, PeerError> {
        self.check_open()?;
        Ok(self.encoded_buffers.get(&buffer).ok_or_else(missing)?.len())
    }
    pub fn create_writable_encoded_buffer(
        &mut self,
        length: usize,
    ) -> Result<SessionWritableEncodedBufferId, PeerError> {
        self.check_open()?;
        let id = SessionWritableEncodedBufferId(next_id()?);
        self.encoded_writers
            .insert(id, WritableEncodedVideoBuffer::new(length));
        Ok(id)
    }
    pub fn resize_encoded_buffer(
        &mut self,
        buffer: SessionWritableEncodedBufferId,
        length: usize,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.encoded_writers
            .get_mut(&buffer)
            .ok_or_else(missing)?
            .reallocate(length);
        Ok(())
    }
    pub fn write_encoded_buffer(
        &mut self,
        buffer: SessionWritableEncodedBufferId,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.encoded_writers
            .get_mut(&buffer)
            .ok_or_else(missing)?
            .write(offset, bytes)
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn writable_encoded_buffer_bytes(
        &mut self,
        buffer: SessionWritableEncodedBufferId,
    ) -> Result<Vec<u8>, PeerError> {
        self.check_open()?;
        self.encoded_writers
            .get(&buffer)
            .ok_or_else(missing)?
            .to_vec()
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))
    }
    pub fn writable_encoded_buffer_len(
        &mut self,
        buffer: SessionWritableEncodedBufferId,
    ) -> Result<usize, PeerError> {
        self.check_open()?;
        Ok(self.encoded_writers.get(&buffer).ok_or_else(missing)?.len())
    }
    /// A failed freeze consumes the writer as the local owning operation does.
    pub fn freeze_encoded_buffer(
        &mut self,
        buffer: SessionWritableEncodedBufferId,
    ) -> Result<SessionEncodedVideoBufferId, PeerError> {
        self.check_open()?;
        let id = SessionEncodedVideoBufferId(next_id()?);
        let value = self
            .encoded_writers
            .remove(&buffer)
            .ok_or_else(missing)?
            .freeze()
            .map_err(|e| error(PeerErrorKind::InvalidParameter, &e.to_string()))?;
        self.encoded_buffers.insert(id, value);
        Ok(id)
    }
    pub fn release_encoded_image(&mut self, id: SessionEncodedImageId) -> Result<(), PeerError> {
        self.check_open()?;
        self.encoded_images.remove(&id).ok_or_else(missing)?;
        Ok(())
    }
    pub fn alias_encoded_image(
        &mut self,
        source: SessionEncodedImageId,
    ) -> Result<SessionEncodedImageId, PeerError> {
        self.check_open()?;
        let value = self
            .encoded_images
            .get(&source)
            .ok_or_else(missing)?
            .clone();
        let id = SessionEncodedImageId(next_id()?);
        self.encoded_images.insert(id, value);
        Ok(id)
    }
    pub fn release_encoded_video_buffer(
        &mut self,
        id: SessionEncodedVideoBufferId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.encoded_buffers.remove(&id).ok_or_else(missing)?;
        Ok(())
    }
    pub fn alias_encoded_video_buffer(
        &mut self,
        source: SessionEncodedVideoBufferId,
    ) -> Result<SessionEncodedVideoBufferId, PeerError> {
        self.check_open()?;
        let value = self
            .encoded_buffers
            .get(&source)
            .ok_or_else(missing)?
            .clone();
        let id = SessionEncodedVideoBufferId(next_id()?);
        self.encoded_buffers.insert(id, value);
        Ok(id)
    }
    pub fn release_writable_encoded_buffer(
        &mut self,
        id: SessionWritableEncodedBufferId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.encoded_writers.remove(&id).ok_or_else(missing)?;
        Ok(())
    }
    pub fn release_encoded_image_callback(
        &mut self,
        id: SessionEncodedImageCallbackId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.encoded_callbacks.remove(&id).ok_or_else(missing)?;
        Ok(())
    }
    pub fn alias_encoded_image_callback(
        &mut self,
        source: SessionEncodedImageCallbackId,
    ) -> Result<SessionEncodedImageCallbackId, PeerError> {
        self.check_open()?;
        let value = self
            .encoded_callbacks
            .get(&source)
            .ok_or_else(missing)?
            .clone();
        let id = SessionEncodedImageCallbackId(next_id()?);
        self.encoded_callbacks.insert(id, value);
        Ok(id)
    }
    pub fn encoded_image_callback_failure_observer(
        &mut self,
        id: SessionEncodedImageCallbackId,
    ) -> Result<VideoCodecCallbackFailureObserver, PeerError> {
        self.check_open()?;
        Ok(self
            .encoded_callbacks
            .get(&id)
            .ok_or_else(missing)?
            .failure_observer())
    }
    pub fn release_decoded_image_callback(
        &mut self,
        id: SessionDecodedImageCallbackId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.decoded_callbacks.remove(&id).ok_or_else(missing)?;
        Ok(())
    }
    pub fn alias_decoded_image_callback(
        &mut self,
        source: SessionDecodedImageCallbackId,
    ) -> Result<SessionDecodedImageCallbackId, PeerError> {
        self.check_open()?;
        let value = self
            .decoded_callbacks
            .get(&source)
            .ok_or_else(missing)?
            .clone();
        let id = SessionDecodedImageCallbackId(next_id()?);
        self.decoded_callbacks.insert(id, value);
        Ok(id)
    }
    pub fn decoded_image_callback_failure_observer(
        &mut self,
        id: SessionDecodedImageCallbackId,
    ) -> Result<VideoCodecCallbackFailureObserver, PeerError> {
        self.check_open()?;
        Ok(self
            .decoded_callbacks
            .get(&id)
            .ok_or_else(missing)?
            .failure_observer())
    }
    pub fn release_fec_override(&mut self, id: SessionFecOverrideId) -> Result<(), PeerError> {
        self.check_open()?;
        self.fec_overrides.remove(&id).ok_or_else(missing)?;
        Ok(())
    }
    pub fn alias_fec_override(
        &mut self,
        source: SessionFecOverrideId,
    ) -> Result<SessionFecOverrideId, PeerError> {
        self.check_open()?;
        let value = self.fec_overrides.get(&source).ok_or_else(missing)?.clone();
        let id = SessionFecOverrideId(next_id()?);
        self.fec_overrides.insert(id, value);
        Ok(id)
    }
    pub fn release_video_encoder_selector(
        &mut self,
        id: SessionVideoEncoderSelectorId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.video_selectors.remove(&id).ok_or_else(missing)?;
        Ok(())
    }
    pub fn alias_video_encoder_selector(
        &mut self,
        source: SessionVideoEncoderSelectorId,
    ) -> Result<SessionVideoEncoderSelectorId, PeerError> {
        self.check_open()?;
        let value = self
            .video_selectors
            .get(&source)
            .ok_or_else(missing)?
            .clone();
        let id = SessionVideoEncoderSelectorId(next_id()?);
        self.video_selectors.insert(id, value);
        Ok(id)
    }
}
