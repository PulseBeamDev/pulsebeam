use std::{fmt, num::NonZeroUsize, rc::Rc};

use crate::{ffi, peer::PeerInner};

/// An owned native DTMF callback. The deprecated callback has no tone-buffer
/// argument, represented by `None`, not by a fabricated current buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DtmfToneChange {
    pub tone: String,
    pub remaining_tones: Option<String>,
}

/// Adapter rejection before native insertion, not a fabricated native result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DtmfInsertError {
    /// A non-comma tone could overflow the pinned signed duration-plus-gap sum.
    DelayOverflow,
}

impl fmt::Display for DtmfInsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("native DTMF duration plus inter-tone gap would overflow")
    }
}
impl std::error::Error for DtmfInsertError {}

/// A sequence-bound native DTMF sender. Retains its peer and execution context.
/// Repeated retrieval for the same RTP sender shares this native handle and its
/// single observer. An audio sender can expose DTMF before telephone-event is
/// negotiated; use `can_insert_tones` to query native capability.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::DtmfSender>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::DtmfSender>();
/// ```
#[derive(Clone)]
pub struct DtmfSender(pub(crate) Rc<DtmfInner>);

pub(crate) struct DtmfInner {
    // Unregister the native observer before releasing the peer/executor.
    native: cxx::UniquePtr<ffi::NativeDtmfSender>,
    _peer: Rc<PeerInner>,
}

impl DtmfSender {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeDtmfSender>,
        peer: Rc<PeerInner>,
    ) -> Self {
        let identity = ffi::dtmf_sender_identity(native.as_ref().unwrap());
        let mut handles = peer.dtmf_senders.borrow_mut();
        handles.retain(|_, weak| weak.strong_count() != 0);
        if let Some(inner) = handles.get(&identity).and_then(std::rc::Weak::upgrade) {
            return Self(inner);
        }
        let inner = Rc::new(DtmfInner {
            native,
            _peer: peer.clone(),
        });
        handles.insert(identity, Rc::downgrade(&inner));
        Self(inner)
    }

    fn native(&self) -> &ffi::NativeDtmfSender {
        self.0
            .native
            .as_ref()
            .expect("validated native DTMF sender")
    }

    pub fn same_resource(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    pub fn can_insert_tones(&self) -> bool {
        ffi::dtmf_sender_can_insert(self.native())
    }

    /// Invoke the native three-argument overload (default comma delay 2000 ms).
    /// Native-invalid durations/gaps return `Ok(false)`. For delay-safe inputs,
    /// unavailable capability also returns the native `Ok(false)` result.
    /// The pinned implementation accepts duration 40..6000 ms and gaps >=30 ms,
    /// not the stricter interface-comment bounds. Unrecognized tones are ignored
    /// by native processing. Successful insertion replaces the previous task.
    pub fn insert_tones(
        &self,
        tones: &str,
        duration_ms: i32,
        gap_ms: i32,
    ) -> Result<bool, DtmfInsertError> {
        self.insert(tones, duration_ms, gap_ms, None)
    }

    /// Invoke the four-argument overload, preserving explicit comma-delay values.
    pub fn insert_tones_with_comma_delay(
        &self,
        tones: &str,
        duration_ms: i32,
        gap_ms: i32,
        comma_delay_ms: i32,
    ) -> Result<bool, DtmfInsertError> {
        self.insert(tones, duration_ms, gap_ms, Some(comma_delay_ms))
    }

    fn insert(
        &self,
        tones: &str,
        duration_ms: i32,
        gap_ms: i32,
        comma_delay_ms: Option<i32>,
    ) -> Result<bool, DtmfInsertError> {
        match ffi::dtmf_sender_insert(
            self.native(),
            tones,
            duration_ms,
            gap_ms,
            comma_delay_ms.is_some(),
            comma_delay_ms.unwrap_or_default(),
        ) {
            0 => Ok(false),
            1 => Ok(true),
            2 => Err(DtmfInsertError::DelayOverflow),
            _ => unreachable!("native adapter returned an unknown DTMF insert result"),
        }
    }

    pub fn tones(&self) -> String {
        ffi::dtmf_sender_tones(self.native())
    }

    pub fn duration_ms(&self) -> i32 {
        ffi::dtmf_sender_duration(self.native())
    }

    pub fn inter_tone_gap_ms(&self) -> i32 {
        ffi::dtmf_sender_inter_tone_gap(self.native())
    }

    pub fn comma_delay_ms(&self) -> i32 {
        ffi::dtmf_sender_comma_delay(self.native())
    }

    /// Register the native observer with a bounded owned event queue. Returns
    /// false without replacing an observer already shared by another handle.
    /// A fresh registration clears prior queued events and overflow counts.
    /// Both native callback overloads are retained as separate events.
    pub fn register_observer(&self, capacity: NonZeroUsize) -> bool {
        ffi::dtmf_sender_register_observer(self.native(), capacity.get())
    }

    /// Detach synchronously before observer destruction. Already queued events
    /// remain available until the next registration or the last handle drop.
    pub fn unregister_observer(&self) {
        ffi::dtmf_sender_unregister_observer(self.native());
    }

    pub fn take_tone_change(&self) -> Option<DtmfToneChange> {
        let event = ffi::dtmf_sender_take_tone_change(self.native());
        event.present.then(|| DtmfToneChange {
            tone: event.tone,
            remaining_tones: event.has_remaining_tones.then_some(event.remaining_tones),
        })
    }

    /// Overflow drops the oldest queued callback. This consumes the saturating
    /// count of callbacks dropped since the previous read or registration.
    pub fn take_dropped_tone_changes(&self) -> u64 {
        ffi::dtmf_sender_take_dropped(self.native())
    }
}

impl fmt::Debug for DtmfSender {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("DtmfSender").finish_non_exhaustive()
    }
}
