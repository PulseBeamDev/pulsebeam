//! Lifetime-bound access to an actual native frame, not a staged copy.
use crate::audio_frame_values::{capacity_shape, enums};
use crate::{
    AudioFrame, AudioFrameFields, AudioFrameSpeechType, AudioFrameVadActivity,
    AudioFrameValueError, ChannelLayout, ffi,
};
use std::{marker::PhantomData, pin::Pin, rc::Rc};

/// Exclusive callback/owner view. This cannot own, free or outlive the frame.
///
/// SDK frames do not initialize their full backing arrays. Sample access is
/// therefore fallible until initialization is proven by this view's writes.
/// Metadata edits never imply initialization. Muted reads use native zero
/// storage; they do not imply that private backing storage is initialized.
/// No data is copied or initialized when this view is constructed.
///
/// The view is neither Send nor Sync. A provider must finish using it before
/// returning; an owning clone is available only for a proven active span.
/// ```compile_fail
/// use pulsebeam_webrtc_sys::AudioFrame;
/// let mut frame = AudioFrame::new();
/// let view = frame.as_mut();
/// drop(frame); // The view retains an exclusive borrow, not an owning pointer.
/// let _ = view.fields();
/// ```
/// ```compile_fail
/// use pulsebeam_webrtc_sys::AudioFrameMut;
/// fn retain_callback_frame<'a>(view: AudioFrameMut<'a>) -> AudioFrameMut<'static> {
///     view // A provider cannot retain a callback borrow as a static resource.
/// }
/// ```
/// ```compile_fail
/// use pulsebeam_webrtc_sys::AudioFrame;
/// fn require_send<T: Send>(_: T) {}
/// let mut frame = AudioFrame::new();
/// require_send(frame.as_mut()); // Callback access stays on its invoking thread.
/// ```
pub struct AudioFrameMut<'a> {
    native: Pin<&'a mut ffi::NativeAudioFrame>,
    initialized_prefix: usize,
    _not_send_sync: PhantomData<Rc<()>>,
}
impl<'a> AudioFrameMut<'a> {
    pub const MAX_DATA_SIZE_SAMPLES: usize = AudioFrame::MAX_DATA_SIZE_SAMPLES;
    pub const MAX_DATA_SIZE_BYTES: usize = AudioFrame::MAX_DATA_SIZE_BYTES;
    pub const MAX_CHANNELS: usize = AudioFrame::MAX_CHANNELS;
    pub const DEFAULT_BUFFER_LENGTH_MS: usize = AudioFrame::DEFAULT_BUFFER_LENGTH_MS;
    pub const DEFAULT_BUFFERS_PER_SEC: usize = AudioFrame::DEFAULT_BUFFERS_PER_SEC;
    /// SDK callback entry does not prove initialization of any private samples.
    pub(crate) fn from_native(native: Pin<&'a mut ffi::NativeAudioFrame>) -> Self {
        Self {
            native,
            initialized_prefix: 0,
            _not_send_sync: PhantomData,
        }
    }
    /// Only independently owned crate frames prove all backing samples.
    pub(crate) fn from_initialized(native: Pin<&'a mut ffi::NativeAudioFrame>) -> Self {
        Self {
            native,
            initialized_prefix: Self::MAX_DATA_SIZE_SAMPLES,
            _not_send_sync: PhantomData,
        }
    }
    fn inner(&self) -> &ffi::NativeAudioFrame {
        self.native.as_ref().get_ref()
    }
    fn active_length(&self) -> Result<usize, AudioFrameValueError> {
        capacity_shape(self.samples_per_channel(), self.num_channels())
    }
    fn require_prefix(&self, length: usize) -> Result<(), AudioFrameValueError> {
        if self.muted() || length <= self.initialized_prefix {
            Ok(())
        } else {
            Err(AudioFrameValueError::UninitializedStorage)
        }
    }
    pub fn fields(&self) -> AudioFrameFields {
        AudioFrameFields::from_native(ffi::media_audio_frame_fields(self.inner()))
    }
    /// Assign public fields only, preserving private state and existing samples.
    pub fn set_fields(&mut self, fields: &AudioFrameFields) -> Result<(), AudioFrameValueError> {
        capacity_shape(fields.samples_per_channel, fields.num_channels)?;
        enums(fields.speech_type, fields.vad_activity)?;
        if !ffi::media_audio_frame_set_fields(self.native.as_mut(), &fields.to_native()) {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(())
    }
    pub fn reset(&mut self) {
        ffi::media_audio_frame_reset(self.native.as_mut(), true);
    }
    pub fn reset_without_muting(&mut self) {
        ffi::media_audio_frame_reset(self.native.as_mut(), false);
    }
    /// Exact native update. Some(data) establishes the copied active prefix;
    /// None mutes without inventing initialization of private backing samples.
    #[allow(clippy::too_many_arguments)]
    pub fn update_frame(
        &mut self,
        timestamp: u32,
        data: Option<&[i16]>,
        samples_per_channel: usize,
        sample_rate_hz: i32,
        speech_type: AudioFrameSpeechType,
        vad_activity: AudioFrameVadActivity,
        num_channels: usize,
    ) -> Result<(), AudioFrameValueError> {
        let length = capacity_shape(samples_per_channel, num_channels)?;
        if num_channels > Self::MAX_CHANNELS {
            return Err(AudioFrameValueError::InvalidShape);
        }
        enums(speech_type, vad_activity)?;
        if data.is_some_and(|value| value.len() < length) {
            return Err(AudioFrameValueError::DataTooShort);
        }
        if !ffi::media_audio_frame_update(
            self.native.as_mut(),
            timestamp,
            data.unwrap_or(&[]),
            data.is_some(),
            samples_per_channel,
            sample_rate_hz,
            speech_type.0,
            vad_activity.0,
            num_channels,
        ) {
            return Err(AudioFrameValueError::InvalidShape);
        }
        if data.is_some() {
            self.initialized_prefix = self.initialized_prefix.max(length);
        }
        Ok(())
    }
    /// Copies an independently owned initialized source using native CopyFrom.
    pub fn copy_from(&mut self, source: &AudioFrame) -> Result<(), AudioFrameValueError> {
        let was_muted = self.muted();
        if !ffi::media_audio_frame_copy(self.native.as_mut(), source.inner()) {
            return Err(AudioFrameValueError::InvalidShape);
        }
        if !source.muted() {
            self.initialized_prefix = if was_muted {
                Self::MAX_DATA_SIZE_SAMPLES
            } else {
                self.initialized_prefix.max(capacity_shape(
                    source.samples_per_channel(),
                    source.num_channels(),
                )?)
            };
        }
        Ok(())
    }
    /// Owned native copy of a proven active view, not of unknown inactive data.
    pub fn try_clone(&self) -> Result<AudioFrame, AudioFrameValueError> {
        self.require_prefix(self.active_length()?)?;
        let mut copy = AudioFrame::new();
        if !ffi::media_audio_frame_copy(copy.native_mut(), self.inner()) {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(copy)
    }
    /// Full capacity only when the buffer is initialized, or native muted zero
    /// storage is selected. Otherwise this returns without constructing a slice.
    pub fn data(&self) -> Result<&[i16], AudioFrameValueError> {
        self.require_prefix(Self::MAX_DATA_SIZE_SAMPLES)?;
        Ok(ffi::media_audio_frame_data(self.inner()))
    }
    pub fn data_view(&self) -> Result<&[i16], AudioFrameValueError> {
        self.require_prefix(self.active_length()?)?;
        let mut valid = false;
        let data = ffi::media_audio_frame_data_view(self.inner(), &mut valid);
        if !valid {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(data)
    }
    /// Native mute-clearing initializes the full buffer. An unmuted buffer
    /// requires full initialization evidence before constructing a Rust slice.
    pub fn mutable_data(&mut self) -> Result<&mut [i16], AudioFrameValueError> {
        self.require_prefix(Self::MAX_DATA_SIZE_SAMPLES)?;
        self.initialized_prefix = Self::MAX_DATA_SIZE_SAMPLES;
        Ok(ffi::media_audio_frame_mutable_data(self.native.as_mut()))
    }
    /// The native shape overload clears only its active prefix when muted.
    /// An unmuted unknown span is refused before changing the native shape.
    pub fn mutable_data_with_shape(
        &mut self,
        samples: usize,
        channels: usize,
    ) -> Result<&mut [i16], AudioFrameValueError> {
        let length = capacity_shape(samples, channels)?;
        if channels > Self::MAX_CHANNELS || channels > samples {
            return Err(AudioFrameValueError::InvalidShape);
        }
        self.require_prefix(length)?;
        let mut valid = false;
        let data = ffi::media_audio_frame_mutable_shape(
            self.native.as_mut(),
            samples,
            channels,
            &mut valid,
        );
        if !valid {
            return Err(AudioFrameValueError::InvalidShape);
        }
        self.initialized_prefix = self.initialized_prefix.max(length);
        Ok(data)
    }
    /// Explicitly initialize a requested native shape before exposing a mutable
    /// span. This does not reset timing, packet information or private layout.
    pub fn fill_data_with_shape(
        &mut self,
        samples: usize,
        channels: usize,
        value: i16,
    ) -> Result<&mut [i16], AudioFrameValueError> {
        let length = capacity_shape(samples, channels)?;
        if channels > Self::MAX_CHANNELS || channels > samples {
            return Err(AudioFrameValueError::InvalidShape);
        }
        let mut valid = false;
        let data = ffi::media_audio_frame_fill_shape(
            self.native.as_mut(),
            samples,
            channels,
            value,
            &mut valid,
        );
        if !valid {
            return Err(AudioFrameValueError::InvalidShape);
        }
        self.initialized_prefix = self.initialized_prefix.max(length);
        Ok(data)
    }
    pub fn mute(&mut self) {
        ffi::media_audio_frame_mute(self.native.as_mut());
    }
    pub fn muted(&self) -> bool {
        ffi::media_audio_frame_muted(self.inner())
    }
    pub fn max_16bit_samples(&self) -> usize {
        ffi::media_audio_frame_max_samples(self.inner())
    }
    pub fn samples_per_channel(&self) -> usize {
        ffi::media_audio_frame_samples_per_channel(self.inner())
    }
    pub fn num_channels(&self) -> usize {
        ffi::media_audio_frame_num_channels(self.inner())
    }
    pub fn channel_layout(&self) -> ChannelLayout {
        ChannelLayout(ffi::media_audio_frame_layout(self.inner()))
    }
    pub fn sample_rate_hz(&self) -> i32 {
        ffi::media_audio_frame_sample_rate(self.inner())
    }
    pub fn set_layout_and_num_channels(
        &mut self,
        layout: ChannelLayout,
        channels: usize,
    ) -> Result<(), AudioFrameValueError> {
        let expected = layout.channel_count()?;
        if expected != 0 && expected != channels {
            return Err(AudioFrameValueError::InvalidLayoutIndex);
        }
        capacity_shape(self.samples_per_channel(), channels)?;
        if !ffi::media_audio_frame_set_layout(self.native.as_mut(), layout.0, channels) {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(())
    }
    pub fn absolute_capture_timestamp_ms(&self) -> Option<i64> {
        let mut present = false;
        let value = ffi::media_audio_frame_capture_timestamp(self.inner(), &mut present);
        present.then_some(value)
    }
    pub fn set_absolute_capture_timestamp_ms(&mut self, timestamp: i64) {
        ffi::media_audio_frame_set_capture_timestamp(self.native.as_mut(), timestamp);
    }
    pub fn set_sample_rate_and_channel_size(
        &mut self,
        rate: i32,
    ) -> Result<(), AudioFrameValueError> {
        if !(0..=192000).contains(&rate) {
            return Err(AudioFrameValueError::InvalidSampleRate);
        }
        capacity_shape(rate as usize / 100, self.num_channels())?;
        if !ffi::media_audio_frame_set_sample_rate(self.native.as_mut(), rate) {
            return Err(AudioFrameValueError::InvalidShape);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioFrameSpeechType as Speech, AudioFrameVadActivity as Vad};

    #[test]
    fn owned_view_preserves_direct_mutation_and_full_initialization() {
        let mut frame = AudioFrame::new();
        {
            let mut view = frame.as_mut();
            view.update_frame(
                17,
                Some(&[1, 2, 3, 4]),
                2,
                8000,
                Speech::NORMAL_SPEECH,
                Vad::ACTIVE,
                2,
            )
            .unwrap();
            assert_eq!(view.data_view().unwrap(), &[1, 2, 3, 4]);
            view.mutable_data().unwrap()[7000] = 19;
            view.set_absolute_capture_timestamp_ms(23);
        }
        assert_eq!(frame.fields().timestamp, 17);
        assert_eq!(frame.data()[7000], 19);
        assert_eq!(frame.absolute_capture_timestamp_ms(), Some(23));
    }

    #[test]
    fn unknown_sdk_storage_is_not_inferred_from_metadata_or_construction() {
        let mut frame = AudioFrame::new();
        frame
            .update_frame(
                31,
                Some(&[1, 2, 3, 4]),
                2,
                8000,
                Speech::NORMAL_SPEECH,
                Vad::ACTIVE,
                2,
            )
            .unwrap();
        let before = frame.fields();
        // Simulate callback evidence, deliberately forgetting the owner's full
        // initialization proof. This tests gates, not uninitialized C++ reads.
        let mut view = AudioFrameMut::from_native(frame.native_mut());
        assert_eq!(view.fields(), before);
        assert_eq!(view.data(), Err(AudioFrameValueError::UninitializedStorage));
        assert_eq!(
            view.data_view(),
            Err(AudioFrameValueError::UninitializedStorage)
        );
        assert_eq!(
            view.interleaved_view().err(),
            Some(AudioFrameValueError::UninitializedStorage)
        );
        assert_eq!(
            view.interleaved_view_mut_with_shape(2, 2).err(),
            Some(AudioFrameValueError::UninitializedStorage)
        );
        assert_eq!(view.fields(), before);
        assert_eq!(
            view.mutable_data(),
            Err(AudioFrameValueError::UninitializedStorage)
        );
        assert_eq!(
            view.try_clone().unwrap_err(),
            AudioFrameValueError::UninitializedStorage
        );
        assert_eq!(view.fill_data_with_shape(2, 2, 7).unwrap(), &[7; 4]);
        assert_eq!(view.data_view().unwrap(), &[7; 4]);
        assert_eq!(&*view.interleaved_view().unwrap(), &[7; 4]);
        assert_eq!(view.data(), Err(AudioFrameValueError::UninitializedStorage));
        let written = view.fields();
        assert_eq!(
            view.mutable_data_with_shape(3, 2),
            Err(AudioFrameValueError::UninitializedStorage)
        );
        assert_eq!(view.fields(), written); // Failed access never changes shape.
        assert_eq!(
            view.interleaved_view_mut_with_shape(3, 2).err(),
            Some(AudioFrameValueError::UninitializedStorage)
        );
        assert_eq!(view.fields(), written);
        let mut fields = view.fields();
        fields.samples_per_channel = 3;
        view.set_fields(&fields).unwrap();
        assert_eq!(
            view.data_view(),
            Err(AudioFrameValueError::UninitializedStorage)
        );
        view.mute();
        assert!(view.data().unwrap().iter().all(|sample| *sample == 0));
        // Full native mute-clearing establishes every backing sample.
        assert!(
            view.mutable_data()
                .unwrap()
                .iter()
                .all(|sample| *sample == 0)
        );
        assert!(!view.muted());
        assert_eq!(view.data_view().unwrap(), &[0; 6]);
    }

    #[test]
    fn copying_only_proves_the_native_written_region() {
        let mut source = AudioFrame::new();
        source
            .update_frame(
                47,
                Some(&[5; 4]),
                2,
                8000,
                Speech::NORMAL_SPEECH,
                Vad::ACTIVE,
                2,
            )
            .unwrap();
        let mut frame = AudioFrame::new();
        frame
            .update_frame(
                0,
                Some(&[0; 4]),
                2,
                8000,
                Speech::NORMAL_SPEECH,
                Vad::ACTIVE,
                2,
            )
            .unwrap();
        let mut view = AudioFrameMut::from_native(frame.native_mut());
        view.copy_from(&source).unwrap();
        assert_eq!(view.data_view().unwrap(), &[5; 4]);
        assert_eq!(view.data(), Err(AudioFrameValueError::UninitializedStorage));
        assert_eq!(view.try_clone().unwrap().fields(), source.fields());
        view.mute();
        // CopyFrom clears the full destination when unmuting a muted frame.
        view.copy_from(&source).unwrap();
        assert_eq!(
            view.data().unwrap().len(),
            AudioFrame::MAX_DATA_SIZE_SAMPLES
        );
        assert_eq!(&view.data().unwrap()[..4], &[5; 4]);
        assert!(view.data().unwrap()[4..].iter().all(|sample| *sample == 0));
    }
}
