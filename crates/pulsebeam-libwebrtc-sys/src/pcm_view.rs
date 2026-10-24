//! Shape-aware PCM borrows. Native frames remain interleaved; planar storage
//! is supplied separately by callers, never guessed from frame metadata.
use crate::{AudioFrame, AudioFrameMut, AudioFrameValueError, ffi};
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PcmViewError {
    InvalidShape,
    NotMono,
    ShapeMismatch,
    EmptyCopy,
    /// The pinned CopySamples template is ill-formed for planar operands.
    NativePlanarCopyUnavailable,
}
impl std::fmt::Display for PcmViewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PCM view: {self:?}")
    }
}
impl std::error::Error for PcmViewError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PcmLayout {
    Mono,
    Interleaved,
    Deinterleaved,
}
/// Immutable dimensions returned by the native view constructor/helpers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PcmViewInfo {
    pub num_channels: usize,
    pub samples_per_channel: usize,
    pub size: usize,
    pub empty: bool,
    pub is_mono: bool,
    pub is_interleaved: bool,
}
impl PcmViewInfo {
    fn native(value: ffi::FfiPcmViewInfo) -> Result<Self, PcmViewError> {
        if !value.valid {
            return Err(PcmViewError::InvalidShape);
        }
        Ok(Self {
            num_channels: value.channels,
            samples_per_channel: value.samples,
            size: value.size,
            empty: value.empty,
            is_mono: value.mono,
            is_interleaved: value.interleaved,
        })
    }
}
mod sealed {
    pub trait View {}
    pub trait Sample {}
}
/// PCM representations supported by these baseline borrowed operations.
/// The trait is sealed; native pointers and adapters are not public APIs.
pub trait PcmSample: sealed::Sample + Copy {
    #[doc(hidden)]
    const IS_F32: bool;
}
impl sealed::Sample for i16 {}
impl sealed::Sample for f32 {}
impl PcmSample for i16 {
    const IS_F32: bool = false;
}
impl PcmSample for f32 {
    const IS_F32: bool = true;
}
fn native_shape<T: PcmSample>(
    data: &[T],
    samples: usize,
    channels: usize,
    layout: u8,
) -> ffi::FfiPcmViewInfo {
    // SAFETY: the sealed sample types and their fixed discriminants prove the
    // cast is exactly the original sample type, not a reinterpretation. Slice
    // allocation, length, initialization, alignment and loan remain unchanged.
    unsafe {
        if T::IS_F32 {
            ffi::pcm_f32_shape(
                std::slice::from_raw_parts(data.as_ptr().cast(), data.len()),
                samples,
                channels,
                layout,
            )
        } else {
            ffi::pcm_i16_shape(
                std::slice::from_raw_parts(data.as_ptr().cast(), data.len()),
                samples,
                channels,
                layout,
            )
        }
    }
}
unsafe fn native_planar_shape<T: PcmSample>(
    pointers: &[usize],
    samples: usize,
) -> ffi::FfiPcmViewInfo {
    if T::IS_F32 {
        unsafe { ffi::pcm_f32_planar_shape(pointers, samples) }
    } else {
        unsafe { ffi::pcm_i16_planar_shape(pointers, samples) }
    }
}
unsafe fn native_clear<T: PcmSample>(pointers: &[usize], samples: usize) {
    if T::IS_F32 {
        unsafe { ffi::pcm_f32_clear(pointers, samples) }
    } else {
        unsafe { ffi::pcm_i16_clear(pointers, samples) }
    }
}
fn native_copy<T: PcmSample>(
    destination: &mut [T],
    source: &[T],
    samples: usize,
    channels: usize,
    dm: bool,
    sm: bool,
    method: bool,
) -> u8 {
    // SAFETY: sealed sample discriminants prove exact element types. Original
    // exclusive/shared slice loans enforce validity and non-overlap until return.
    unsafe {
        if T::IS_F32 {
            ffi::pcm_f32_copy(
                std::slice::from_raw_parts_mut(destination.as_mut_ptr().cast(), destination.len()),
                std::slice::from_raw_parts(source.as_ptr().cast(), source.len()),
                samples,
                channels,
                dm,
                sm,
                method,
            )
        } else {
            ffi::pcm_i16_copy(
                std::slice::from_raw_parts_mut(destination.as_mut_ptr().cast(), destination.len()),
                std::slice::from_raw_parts(source.as_ptr().cast(), source.len()),
                samples,
                channels,
                dm,
                sm,
                method,
            )
        }
    }
}

/// Common shape helpers. Implementations are sealed to preserve borrow and
/// shape proofs across native CopySamples calls.
pub trait PcmRead: sealed::View {
    type Sample: PcmSample;
    fn info(&self) -> PcmViewInfo;
    fn layout(&self) -> PcmLayout;
    #[doc(hidden)]
    fn flat_samples(&self) -> Option<&[Self::Sample]>;
    fn num_channels(&self) -> usize {
        self.info().num_channels
    }
    fn samples_per_channel(&self) -> usize {
        self.info().samples_per_channel
    }
    fn size(&self) -> usize {
        self.info().size
    }
    fn is_empty(&self) -> bool {
        self.info().empty
    }
    fn is_mono(&self) -> bool {
        self.info().is_mono
    }
    fn is_interleaved(&self) -> bool {
        self.info().is_interleaved
    }
}
pub trait PcmWrite: PcmRead {
    #[doc(hidden)]
    fn flat_samples_mut(&mut self) -> Option<&mut [Self::Sample]>;
}
fn shape<T: PcmSample>(
    data: &[T],
    samples: usize,
    channels: usize,
    layout: u8,
) -> Result<PcmViewInfo, PcmViewError> {
    PcmViewInfo::native(native_shape(data, samples, channels, layout))
}

#[derive(Clone, Copy, Debug)]
pub struct MonoPcm<'a, T: PcmSample = i16> {
    data: &'a [T],
    info: PcmViewInfo,
}
pub struct MonoPcmMut<'a, T: PcmSample = i16> {
    data: &'a mut [T],
    info: PcmViewInfo,
}
#[derive(Clone, Copy, Debug)]
pub struct InterleavedPcm<'a, T: PcmSample = i16> {
    data: &'a [T],
    info: PcmViewInfo,
}
/// Exclusive typed borrow, including callback frame borrows.
/// ```compile_fail
/// use pulsebeam_webrtc_sys::AudioFrame;
/// let mut frame = AudioFrame::new();
/// let mut view = frame.interleaved_view_mut_with_shape(4, 1).unwrap();
/// frame.mute(); // The frame cannot mutate or free the storage during this loan.
/// view[0] = 1;
/// ```
pub struct InterleavedPcmMut<'a, T: PcmSample = i16> {
    data: &'a mut [T],
    info: PcmViewInfo,
}
macro_rules! flat_view {
    ($name:ident, $layout:ident) => {
        impl<T: PcmSample> sealed::View for $name<'_, T> {}
        impl<T: PcmSample> PcmRead for $name<'_, T> {
            type Sample = T;
            fn info(&self) -> PcmViewInfo {
                self.info
            }
            fn layout(&self) -> PcmLayout {
                PcmLayout::$layout
            }
            fn flat_samples(&self) -> Option<&[T]> {
                Some(self.data)
            }
        }
        impl<T: PcmSample> Deref for $name<'_, T> {
            type Target = [T];
            fn deref(&self) -> &[T] {
                self.data
            }
        }
    };
}
flat_view!(MonoPcm, Mono);
flat_view!(MonoPcmMut, Mono);
flat_view!(InterleavedPcm, Interleaved);
flat_view!(InterleavedPcmMut, Interleaved);
macro_rules! flat_mut {
    ($name:ident) => {
        impl<T: PcmSample> PcmWrite for $name<'_, T> {
            fn flat_samples_mut(&mut self) -> Option<&mut [T]> {
                Some(self.data)
            }
        }
        impl<T: PcmSample> DerefMut for $name<'_, T> {
            fn deref_mut(&mut self) -> &mut [T] {
                self.data
            }
        }
    };
}
flat_mut!(MonoPcmMut);
flat_mut!(InterleavedPcmMut);
impl<T: PcmSample> Default for InterleavedPcm<'_, T> {
    fn default() -> Self {
        Self::new(&[], 0, 0).expect("native empty interleaved view")
    }
}
impl<T: PcmSample> Default for InterleavedPcmMut<'_, T> {
    fn default() -> Self {
        Self::new(&mut [], 0, 0).expect("native empty interleaved view")
    }
}
impl<T: PcmSample> Default for DeinterleavedPcm<'_, T> {
    fn default() -> Self {
        Self::channels(Vec::new(), 0).expect("native empty planar view")
    }
}
impl<T: PcmSample> Default for DeinterleavedPcmMut<'_, T> {
    fn default() -> Self {
        Self::channels(Vec::new(), 0).expect("native empty planar view")
    }
}
impl<'a, T: PcmSample> MonoPcm<'a, T> {
    pub fn new(data: &'a [T]) -> Self {
        Self {
            info: shape(data, data.len(), 1, 0).expect("native mono span"),
            data,
        }
    }
}
impl<'a, T: PcmSample> MonoPcmMut<'a, T> {
    pub fn new(data: &'a mut [T]) -> Self {
        Self {
            info: shape(data, data.len(), 1, 0).expect("native mono span"),
            data,
        }
    }
    pub fn as_const(&self) -> MonoPcm<'_, T> {
        MonoPcm {
            data: self.data,
            info: self.info,
        }
    }
}
impl<'a, T: PcmSample> InterleavedPcm<'a, T> {
    /// Pointer-constructor semantics: only the specified active prefix is
    /// borrowed; extra backing samples are neither exposed nor normalized.
    pub fn new(data: &'a [T], samples: usize, channels: usize) -> Result<Self, PcmViewError> {
        let info = shape(data, samples, channels, 1)?;
        Ok(Self {
            data: &data[..info.size],
            info,
        })
    }
    /// Array/span convenience constructor. Rejects zero divisors/remainders
    /// before entering the native array-constructor domain.
    pub fn from_samples(data: &'a [T], channels: usize) -> Result<Self, PcmViewError> {
        if channels == 0 || !data.len().is_multiple_of(channels) {
            return Err(PcmViewError::InvalidShape);
        }
        Self::new(data, data.len() / channels, channels)
    }
    pub fn as_mono(&self) -> Result<MonoPcm<'_, T>, PcmViewError> {
        if !self.is_mono() {
            return Err(PcmViewError::NotMono);
        }
        Ok(MonoPcm {
            data: self.data,
            info: shape(self.data, self.len(), 1, 0)?,
        })
    }
}
impl<'a, T: PcmSample> InterleavedPcmMut<'a, T> {
    pub fn new(data: &'a mut [T], samples: usize, channels: usize) -> Result<Self, PcmViewError> {
        let info = shape(data, samples, channels, 1)?;
        Ok(Self {
            data: &mut data[..info.size],
            info,
        })
    }
    pub fn from_samples(data: &'a mut [T], channels: usize) -> Result<Self, PcmViewError> {
        if channels == 0 || !data.len().is_multiple_of(channels) {
            return Err(PcmViewError::InvalidShape);
        }
        let samples = data.len() / channels;
        Self::new(data, samples, channels)
    }
    pub fn as_const(&self) -> InterleavedPcm<'_, T> {
        InterleavedPcm {
            data: self.data,
            info: self.info,
        }
    }
    pub fn as_mono_mut(&mut self) -> Result<MonoPcmMut<'_, T>, PcmViewError> {
        if !self.is_mono() {
            return Err(PcmViewError::NotMono);
        }
        let info = shape(self.data, self.data.len(), 1, 0)?;
        Ok(MonoPcmMut {
            data: self.data,
            info,
        })
    }
    /// Actual native InterleavedView::CopyFrom, not a replacement copy loop.
    pub fn copy_from(&mut self, source: &InterleavedPcm<'_, T>) -> Result<(), PcmViewError> {
        copy_flat(self, source, true)
    }
}

#[derive(Clone, Debug)]
enum Planar<'a, T: PcmSample> {
    Contiguous(&'a [T]),
    Channels(Vec<&'a [T]>),
}
enum PlanarMut<'a, T: PcmSample> {
    Contiguous(&'a mut [T]),
    Channels(Vec<&'a mut [T]>),
}
#[derive(Clone, Debug)]
pub struct DeinterleavedPcm<'a, T: PcmSample = i16> {
    data: Planar<'a, T>,
    info: PcmViewInfo,
}
/// Exclusive channel borrows cannot alias or outlive their backing storage.
/// ```compile_fail
/// use pulsebeam_webrtc_sys::DeinterleavedPcmMut;
/// let mut samples = [0i16; 4];
/// let _view = DeinterleavedPcmMut::channels(vec![&mut samples, &mut samples], 4);
/// ```
pub struct DeinterleavedPcmMut<'a, T: PcmSample = i16> {
    data: PlanarMut<'a, T>,
    info: PcmViewInfo,
}
fn channel_shape<T: PcmSample>(
    planes: &[&[T]],
    samples: usize,
) -> Result<PcmViewInfo, PcmViewError> {
    samples
        .checked_mul(planes.len())
        .ok_or(PcmViewError::InvalidShape)?;
    if planes.iter().any(|p| p.len() < samples) {
        return Err(PcmViewError::InvalidShape);
    }
    let pointers: Vec<usize> = planes.iter().map(|p| p.as_ptr() as usize).collect();
    // SAFETY: every plane is borrowed and initialized for the declared prefix.
    // Native metadata access is synchronous and retains no pointer array.
    PcmViewInfo::native(unsafe { native_planar_shape::<T>(&pointers, samples) })
}
impl<'a, T: PcmSample> DeinterleavedPcm<'a, T> {
    pub fn coallocated(
        data: &'a [T],
        samples: usize,
        channels: usize,
    ) -> Result<Self, PcmViewError> {
        let info = shape(data, samples, channels, 2)?;
        Ok(Self {
            data: Planar::Contiguous(&data[..info.size]),
            info,
        })
    }
    pub fn channels(planes: Vec<&'a [T]>, samples: usize) -> Result<Self, PcmViewError> {
        let info = channel_shape(&planes, samples)?;
        Ok(Self {
            data: Planar::Channels(planes),
            info,
        })
    }
    pub fn channel(&self, index: usize) -> Option<MonoPcm<'_, T>> {
        if index >= self.num_channels() {
            return None;
        }
        let n = self.samples_per_channel();
        let data = match &self.data {
            Planar::Contiguous(data) => &data[index * n..(index + 1) * n],
            Planar::Channels(data) => &data[index][..n],
        };
        Some(MonoPcm::new(data))
    }
    /// Native planar AsMono returns the first channel even in a multi-channel
    /// view; unlike interleaved AsMono it does not require IsMono.
    pub fn as_mono(&self) -> Result<MonoPcm<'_, T>, PcmViewError> {
        self.channel(0).ok_or(PcmViewError::NotMono)
    }
}
impl<'a, T: PcmSample> DeinterleavedPcmMut<'a, T> {
    pub fn coallocated(
        data: &'a mut [T],
        samples: usize,
        channels: usize,
    ) -> Result<Self, PcmViewError> {
        let info = shape(data, samples, channels, 2)?;
        Ok(Self {
            data: PlanarMut::Contiguous(&mut data[..info.size]),
            info,
        })
    }
    pub fn channels(planes: Vec<&'a mut [T]>, samples: usize) -> Result<Self, PcmViewError> {
        let borrows: Vec<&[T]> = planes.iter().map(|p| &p[..]).collect();
        let info = channel_shape(&borrows, samples)?;
        Ok(Self {
            data: PlanarMut::Channels(planes),
            info,
        })
    }
    pub fn as_const(&self) -> DeinterleavedPcm<'_, T> {
        let data = match &self.data {
            PlanarMut::Contiguous(data) => Planar::Contiguous(data),
            PlanarMut::Channels(data) => Planar::Channels(data.iter().map(|p| &p[..]).collect()),
        };
        DeinterleavedPcm {
            data,
            info: self.info,
        }
    }
    pub fn channel(&self, index: usize) -> Option<MonoPcm<'_, T>> {
        if index >= self.num_channels() {
            return None;
        }
        let n = self.samples_per_channel();
        let data = match &self.data {
            PlanarMut::Contiguous(data) => &data[index * n..(index + 1) * n],
            PlanarMut::Channels(data) => &data[index][..n],
        };
        Some(MonoPcm::new(data))
    }
    pub fn channel_mut(&mut self, index: usize) -> Option<MonoPcmMut<'_, T>> {
        if index >= self.num_channels() {
            return None;
        }
        let n = self.samples_per_channel();
        let data = match &mut self.data {
            PlanarMut::Contiguous(data) => &mut data[index * n..(index + 1) * n],
            PlanarMut::Channels(data) => &mut data[index][..n],
        };
        Some(MonoPcmMut::new(data))
    }
    pub fn as_mono_mut(&mut self) -> Result<MonoPcmMut<'_, T>, PcmViewError> {
        self.channel_mut(0).ok_or(PcmViewError::NotMono)
    }
    /// Native DeinterleavedView::Clear over the exact borrowed channel prefixes.
    pub fn clear(&mut self) {
        let n = self.samples_per_channel();
        let pointers: Vec<usize> = match &mut self.data {
            PlanarMut::Contiguous(data) => (0..self.info.num_channels)
                .map(|i| data[i * n..].as_mut_ptr() as usize)
                .collect(),
            PlanarMut::Channels(data) => data.iter_mut().map(|p| p.as_mut_ptr() as usize).collect(),
        };
        // SAFETY: the exclusive Rust borrows keep all channel prefixes valid
        // and disjoint until the synchronous native Clear returns.
        unsafe {
            native_clear::<T>(&pointers, n);
        }
    }
}
macro_rules! planar_view {
    ($name:ident) => {
        impl<T: PcmSample> sealed::View for $name<'_, T> {}
        impl<T: PcmSample> PcmRead for $name<'_, T> {
            type Sample = T;
            fn info(&self) -> PcmViewInfo {
                self.info
            }
            fn layout(&self) -> PcmLayout {
                PcmLayout::Deinterleaved
            }
            fn flat_samples(&self) -> Option<&[T]> {
                None
            }
        }
    };
}
planar_view!(DeinterleavedPcm);
planar_view!(DeinterleavedPcmMut);
impl<T: PcmSample> PcmWrite for DeinterleavedPcmMut<'_, T> {
    fn flat_samples_mut(&mut self) -> Option<&mut [T]> {
        None
    }
}
fn copy_flat<D: PcmWrite, S: PcmRead<Sample = D::Sample>>(
    destination: &mut D,
    source: &S,
    method: bool,
) -> Result<(), PcmViewError> {
    if !destination.is_interleaved() || !source.is_interleaved() {
        return Err(PcmViewError::NativePlanarCopyUnavailable);
    }
    if destination.num_channels() != source.num_channels()
        || destination.samples_per_channel() != source.samples_per_channel()
    {
        return Err(PcmViewError::ShapeMismatch);
    }
    if source.is_empty() {
        return Err(PcmViewError::EmptyCopy);
    }
    let dm = destination.layout() == PcmLayout::Mono;
    let sm = source.layout() == PcmLayout::Mono;
    let samples = source.samples_per_channel();
    let channels = source.num_channels();
    let code = native_copy(
        destination.flat_samples_mut().expect("sealed flat view"),
        source.flat_samples().expect("sealed flat view"),
        samples,
        channels,
        dm,
        sm,
        method,
    );
    match code {
        0 => Ok(()),
        2 => Err(PcmViewError::EmptyCopy),
        _ => Err(PcmViewError::ShapeMismatch),
    }
}
/// Actual native CopySamples for supported mono/interleaved combinations.
/// Empty copies are rejected before the pinned helper forms element-zero
/// addresses. Planar instantiation is genuinely unavailable in this SDK.
pub fn copy_samples<D: PcmWrite, S: PcmRead<Sample = D::Sample>>(
    destination: &mut D,
    source: &S,
) -> Result<(), PcmViewError> {
    copy_flat(destination, source, false)
}

impl AudioFrame {
    pub fn interleaved_view(&self) -> Result<InterleavedPcm<'_>, AudioFrameValueError> {
        let (n, c) = (self.samples_per_channel(), self.num_channels());
        InterleavedPcm::new(self.data_view()?, n, c).map_err(|_| AudioFrameValueError::InvalidShape)
    }
    /// Shape-taking native mutable_data semantics, including prefix-only
    /// mute clearing and existing shape validation before mutation.
    pub fn interleaved_view_mut_with_shape(
        &mut self,
        samples: usize,
        channels: usize,
    ) -> Result<InterleavedPcmMut<'_>, AudioFrameValueError> {
        InterleavedPcmMut::new(
            self.mutable_data_with_shape(samples, channels)?,
            samples,
            channels,
        )
        .map_err(|_| AudioFrameValueError::InvalidShape)
    }
}
impl AudioFrameMut<'_> {
    pub fn interleaved_view(&self) -> Result<InterleavedPcm<'_>, AudioFrameValueError> {
        let (n, c) = (self.samples_per_channel(), self.num_channels());
        InterleavedPcm::new(self.data_view()?, n, c).map_err(|_| AudioFrameValueError::InvalidShape)
    }
    pub fn interleaved_view_mut_with_shape(
        &mut self,
        samples: usize,
        channels: usize,
    ) -> Result<InterleavedPcmMut<'_>, AudioFrameValueError> {
        InterleavedPcmMut::new(
            self.mutable_data_with_shape(samples, channels)?,
            samples,
            channels,
        )
        .map_err(|_| AudioFrameValueError::InvalidShape)
    }
}
impl crate::AudioPcmFrame {
    pub fn interleaved_view(&self) -> Result<InterleavedPcm<'_>, PcmViewError> {
        InterleavedPcm::new(
            &self.samples,
            self.samples_per_channel as usize,
            self.channels as usize,
        )
    }
}
impl crate::DecodedAudioFrame {
    pub fn interleaved_view(&self) -> Result<InterleavedPcm<'_>, PcmViewError> {
        InterleavedPcm::new(
            &self.samples,
            self.samples_per_channel as usize,
            self.channels as usize,
        )
    }
}
