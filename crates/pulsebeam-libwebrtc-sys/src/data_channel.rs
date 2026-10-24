use std::{fmt, rc::Rc};

use crate::{PeerConnection, PeerError, PeerErrorKind, ffi, peer::PeerInner};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataChannelPriority {
    VeryLow,
    Low,
    Medium,
    High,
    /// Any native 16-bit priority value, without narrowing to four presets.
    Custom(u16),
}

impl DataChannelPriority {
    pub fn value(self) -> u16 {
        match self {
            Self::VeryLow => 128,
            Self::Low => 256,
            Self::Medium => 512,
            Self::High => 1024,
            Self::Custom(value) => value,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataChannelConfiguration {
    /// Deprecated native initialization flag, ignored by the pinned engine.
    /// Configuration snapshots instead expose the actual `reliable()` property.
    pub reliable: bool,
    pub ordered: bool,
    /// Native signed input: negative values disable the setting, and values
    /// above 65535 are clamped by the engine before mutual-exclusion validation.
    pub max_retransmit_time_ms: Option<i32>,
    pub max_retransmits: Option<i32>,
    pub protocol: Vec<u8>,
    pub negotiated: bool,
    /// Explicit SCTP stream identity for external negotiation. `None` lets
    /// native negotiation assign it. The pinned engine rejects the reserved
    /// value 65535 with `InvalidRange`; an in-band id is ignored natively.
    pub id: Option<i32>,
    pub priority: Option<DataChannelPriority>,
}

impl Default for DataChannelConfiguration {
    fn default() -> Self {
        Self {
            reliable: false,
            ordered: true,
            max_retransmit_time_ms: None,
            max_retransmits: None,
            protocol: Vec::new(),
            negotiated: false,
            id: None,
            priority: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataChannelState {
    Connecting,
    Open,
    Closing,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataChannelMessageKind {
    Text,
    Binary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataChannelMessage {
    pub kind: DataChannelMessageKind,
    pub bytes: Vec<u8>,
}

impl DataChannelMessage {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: DataChannelMessageKind::Text,
            bytes: text.into().into_bytes(),
        }
    }

    pub fn binary(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            kind: DataChannelMessageKind::Binary,
            bytes: bytes.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataChannelSendResult {
    Sent,
    NotOpen,
    Backpressure,
    Failed,
}

/// An owned snapshot of the native data-channel error. `None` from
/// [`DataChannel::error`] means the engine reports no error, including an
/// ordinary explicit close. Message bytes are retained exactly; their diagnostic
/// wording is not a stable API. `Display` is a lossy UTF-8 convenience only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataChannelError {
    pub kind: PeerErrorKind,
    pub message: Vec<u8>,
    pub detail: Option<DataChannelErrorDetail>,
    pub sctp_cause_code: Option<u16>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataChannelErrorDetail {
    DataChannelFailure,
    DtlsFailure,
    FingerprintFailure,
    SctpFailure,
    SdpSyntaxError,
    HardwareEncoderNotAvailable,
    HardwareEncoderError,
    Unknown(u8),
}

impl fmt::Display for DataChannelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&String::from_utf8_lossy(&self.message))
    }
}
impl std::error::Error for DataChannelError {}

/// Channel-local identity of an admitted asynchronous send. Identities are not
/// recycled; use the originating channel together with this value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DataChannelSendId(u64);

impl DataChannelSendId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataChannelSendError {
    /// The engine completed the send with this error.
    Native(DataChannelError),
    /// The engine discarded the completion callback without running the send,
    /// for example after transport detachment. Not a synthesized native error.
    Canceled,
}

impl fmt::Display for DataChannelSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native(error) => error.fmt(formatter),
            Self::Canceled => formatter.write_str("data-channel send canceled"),
        }
    }
}
impl std::error::Error for DataChannelSendError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataChannelSendCompletion {
    pub send_id: DataChannelSendId,
    /// Success means local native send admission, not remote delivery.
    pub result: Result<(), DataChannelSendError>,
}

/// Native cumulative channel counters. Each field is sampled independently,
/// so concurrent transport activity can occur between field reads. Receiving
/// counts native deliveries, not consumption of the Rust event inbox.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DataChannelCounters {
    pub messages_sent: u32,
    pub bytes_sent: u64,
    pub messages_received: u32,
    pub bytes_received: u64,
}

/// Bounds on the owned message inbox, independent of native SCTP buffering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DataChannelReceiveQueueLimits {
    pub max_messages: usize,
    pub max_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataChannelEvent {
    /// Native deliveries rejected by the full owned inbox since the previous
    /// overflow event. Newest messages are dropped before copying. Counts
    /// saturate at `u64::MAX`; this does not signal a native transport failure.
    MessagesDropped {
        messages: u64,
        bytes: u64,
    },
    /// Latest callback-time state snapshot. Intermediate states may coalesce;
    /// this is not a historical transition sampled at polling time.
    StateChanged(DataChannelState),
    Message(DataChannelMessage),
    /// Latest native negotiated limit in bytes, coalesced independently of
    /// messages. Preserves the native observer value, including zero.
    MaxMessageSizeChanged {
        max_message_size: i32,
    },
    /// Exactly one outcome per admitted send while this channel is retained.
    /// Completions remain available after close and when observation is disabled.
    SendComplete(DataChannelSendCompletion),
    /// Coalesced bytes removed from the local send queue since the last such
    /// event, saturating at `u64::MAX`. This is not remote delivery evidence.
    BufferedAmountChanged {
        sent_data_size: u64,
    },
}

/// A sequence-bound data channel with a caller-polled owned event queue.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::DataChannel>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::DataChannel>();
/// ```
#[derive(Clone)]
pub struct DataChannel(Rc<DataChannelInner>);

struct DataChannelInner {
    // The last alias unregisters the observer before releasing the peer roles.
    native: cxx::UniquePtr<ffi::NativeDataChannel>,
    _peer: Rc<PeerInner>,
}

impl fmt::Debug for DataChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DataChannel")
            .field("label", &self.label())
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl PeerConnection {
    pub fn create_data_channel(
        &self,
        label: &str,
        configuration: DataChannelConfiguration,
    ) -> Result<DataChannel, crate::PeerErrorBytes> {
        self.create_data_channel_bytes(label.as_bytes(), Some(configuration))
    }

    /// `None` passes a null configuration to the native creation operation.
    /// Labels, protocols and native error diagnostics retain their actual bytes.
    pub fn create_data_channel_bytes(
        &self,
        label: &[u8],
        configuration: Option<DataChannelConfiguration>,
    ) -> Result<DataChannel, crate::PeerErrorBytes> {
        let has_configuration = configuration.is_some();
        let configuration = configuration.unwrap_or_default();
        let fields = ffi::FfiDataChannelInit {
            has_configuration,
            reliable: configuration.reliable,
            ordered: configuration.ordered,
            has_max_retransmit_time_ms: configuration.max_retransmit_time_ms.is_some(),
            max_retransmit_time_ms: configuration.max_retransmit_time_ms.unwrap_or(0),
            has_max_retransmits: configuration.max_retransmits.is_some(),
            max_retransmits: configuration.max_retransmits.unwrap_or(0),
            protocol: configuration.protocol,
            negotiated: configuration.negotiated,
            id: configuration.id.unwrap_or(-1),
            priority: configuration
                .priority
                .map_or(-1, |value| i32::from(value.value())),
        };
        let mut error_type = 0;
        let mut message = Vec::new();
        let native = ffi::create_data_channel(
            self.inner.native(),
            label,
            &fields,
            &mut error_type,
            &mut message,
        );
        if native.is_null() {
            Err(crate::PeerErrorBytes {
                kind: super::peer::error_kind(error_type),
                message,
            })
        } else {
            Ok(DataChannel::from_native(native, self.inner.clone()))
        }
    }
}

impl DataChannel {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeDataChannel>,
        peer: Rc<PeerInner>,
    ) -> Self {
        Self(Rc::new(DataChannelInner {
            native,
            _peer: peer,
        }))
    }

    /// Aliases share one observer, inbox and asynchronous send budget. Dropping
    /// an alias does not close the channel until its final owner is released.
    pub fn same_resource(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    pub fn receive_queue_limits() -> DataChannelReceiveQueueLimits {
        let limits = ffi::data_channel_receive_queue_limits();
        DataChannelReceiveQueueLimits {
            max_messages: limits.max_messages,
            max_bytes: limits.max_bytes,
        }
    }

    /// Owned native bytes, without assuming UTF-8 or a C-string terminator.
    pub fn label(&self) -> Vec<u8> {
        ffi::data_channel_label(self.native())
    }

    /// Actual native getter values, not an echo of the initialization request.
    /// In particular, reliability is effective, and an in-band id is assigned
    /// by the engine rather than copied from the request.
    pub fn configuration(&self) -> DataChannelConfiguration {
        DataChannelConfiguration {
            reliable: self.reliable(),
            ordered: ffi::data_channel_ordered(self.native()),
            max_retransmit_time_ms: optional_i32(ffi::data_channel_max_retransmit_time_ms(
                self.native(),
            )),
            max_retransmits: optional_i32(ffi::data_channel_max_retransmits(self.native())),
            protocol: ffi::data_channel_protocol(self.native()),
            negotiated: ffi::data_channel_negotiated(self.native()),
            id: optional_i32(ffi::data_channel_id(self.native())),
            priority: Some(priority(ffi::data_channel_priority(self.native()))),
        }
    }

    pub fn state(&self) -> DataChannelState {
        state(ffi::data_channel_state(self.native()))
    }

    /// Native reliability property, not an inference from ordered delivery.
    pub fn reliable(&self) -> bool {
        ffi::data_channel_reliable(self.native())
    }

    pub fn counters(&self) -> DataChannelCounters {
        let value = ffi::data_channel_counters(self.native());
        DataChannelCounters {
            messages_sent: value.messages_sent,
            bytes_sent: value.bytes_sent,
            messages_received: value.messages_received,
            bytes_received: value.bytes_received,
        }
    }

    pub fn buffered_amount(&self) -> u64 {
        ffi::data_channel_buffered_amount(self.native())
    }

    /// Native per-channel send-queue limit, not a maximum negotiated message
    /// size and not a receive budget. This value comes from the pinned engine.
    pub fn send_queue_capacity() -> u64 {
        ffi::data_channel_send_queue_capacity()
    }

    /// Current native error copied into an owned Rust value. This query does
    /// not drive protocol progress or fabricate a failure for a slow consumer.
    pub fn error(&self) -> Option<DataChannelError> {
        native_error(ffi::data_channel_error(self.native()))
    }

    /// Queue a native asynchronous send. At most 64 sends per channel may be
    /// outstanding, including completed outcomes not yet consumed. Oversized
    /// input above the engine's queue capacity is rejected before copying.
    /// Admission is not native send success: consume `SendComplete` events for
    /// native errors, including invalid state or negotiated message-size limits.
    /// Callbacks may complete before this method returns without losing results.
    pub fn send_async(&self, message: DataChannelMessage) -> Result<DataChannelSendId, PeerError> {
        if message.bytes.len() as u64 > Self::send_queue_capacity() {
            return Err(PeerError {
                kind: PeerErrorKind::ResourceExhausted,
                message: "message exceeds the native data-channel send queue capacity".into(),
            });
        }
        let id = ffi::data_channel_send_async(
            self.native(),
            &message.bytes,
            message.kind == DataChannelMessageKind::Binary,
        );
        if id == 0 {
            Err(PeerError {
                kind: PeerErrorKind::ResourceExhausted,
                message: "data-channel asynchronous send admission exhausted".into(),
            })
        } else {
            Ok(DataChannelSendId(id))
        }
    }

    /// Register or unregister the native observer. Observation starts enabled.
    /// Disabling quiesces new channel callbacks but retains already-owned Rust
    /// events. Native buffering and possible overflow remain engine behavior;
    /// this is not a receive-window credit or transport pause operation.
    ///
    /// Re-enabling drains native queued messages when the channel is open and
    /// publishes a current-state snapshot. While disabled, observer callbacks do
    /// not notify readiness; asynchronous send completions still do. Queries such
    /// as `state` and `error` still reach the native handle.
    pub fn set_event_observation(&mut self, enabled: bool) {
        ffi::data_channel_set_event_observation(self.native(), enabled);
    }

    /// `Sent` denotes native local admission, not remote application delivery.
    pub fn send(&self, message: DataChannelMessage) -> DataChannelSendResult {
        match ffi::data_channel_send(
            self.native(),
            &message.bytes,
            message.kind == DataChannelMessageKind::Binary,
        ) {
            0 => DataChannelSendResult::Sent,
            1 => DataChannelSendResult::NotOpen,
            2 => DataChannelSendResult::Backpressure,
            3 => DataChannelSendResult::Failed,
            _ => unreachable!("native adapter returned an invalid send result"),
        }
    }

    /// Take an owned event without driving native progress. At most one state
    /// snapshot, one buffered-amount notification and one message-size snapshot
    /// are retained independently of message storage. Send completions precede
    /// advisory notifications and queued messages; each queue is FIFO. A closed
    /// snapshot clears remaining message deliveries and advisory notifications.
    /// Outstanding send completions and overflow notifications remain observable
    /// after close. The message inbox is bounded by `receive_queue_limits`;
    /// newest deliveries exceeding either limit produce `MessagesDropped`.
    /// Native reception is push-based: polling drains Rust storage and does not
    /// return SCTP receive-window credit. Overflow does not close the channel.
    pub fn try_next_event(&self) -> Option<DataChannelEvent> {
        let event = ffi::data_channel_take_event(self.native());
        match event.kind {
            0 => None,
            6 => Some(DataChannelEvent::MessagesDropped {
                messages: event.dropped_messages,
                bytes: event.dropped_bytes,
            }),
            1 => Some(DataChannelEvent::StateChanged(state(event.state))),
            2 => Some(DataChannelEvent::Message(DataChannelMessage {
                kind: if event.binary {
                    DataChannelMessageKind::Binary
                } else {
                    DataChannelMessageKind::Text
                },
                bytes: event.data,
            })),
            3 => Some(DataChannelEvent::BufferedAmountChanged {
                sent_data_size: event.sent_data_size,
            }),
            4 => Some(DataChannelEvent::MaxMessageSizeChanged {
                max_message_size: event.max_message_size,
            }),
            5 => Some(DataChannelEvent::SendComplete(DataChannelSendCompletion {
                send_id: DataChannelSendId(event.send_id),
                result: if event.canceled {
                    Err(DataChannelSendError::Canceled)
                } else {
                    native_error(event.error)
                        .map_or(Ok(()), |error| Err(DataChannelSendError::Native(error)))
                },
            })),
            _ => unreachable!("native adapter returned an invalid data-channel event"),
        }
    }

    pub fn close(&mut self) -> Result<(), PeerError> {
        ffi::close_data_channel(self.native())
            .then_some(())
            .ok_or_else(|| PeerError {
                kind: PeerErrorKind::Internal,
                message: "failed to close data channel".into(),
            })
    }

    fn native(&self) -> &ffi::NativeDataChannel {
        self.0.native.as_ref().expect("validated data channel")
    }
}

fn native_error(error: ffi::FfiDataChannelError) -> Option<DataChannelError> {
    error.has_error.then(|| DataChannelError {
        kind: crate::peer::error_kind(error.error_type),
        message: error.message,
        detail: match error.error_detail {
            0 => None,
            1 => Some(DataChannelErrorDetail::DataChannelFailure),
            2 => Some(DataChannelErrorDetail::DtlsFailure),
            3 => Some(DataChannelErrorDetail::FingerprintFailure),
            4 => Some(DataChannelErrorDetail::SctpFailure),
            5 => Some(DataChannelErrorDetail::SdpSyntaxError),
            6 => Some(DataChannelErrorDetail::HardwareEncoderNotAvailable),
            7 => Some(DataChannelErrorDetail::HardwareEncoderError),
            value => Some(DataChannelErrorDetail::Unknown(value)),
        },
        sctp_cause_code: (error.sctp_cause_code >= 0).then_some(error.sctp_cause_code as u16),
    })
}

fn optional_i32(value: i32) -> Option<i32> {
    (value >= 0).then_some(value)
}

fn state(value: u8) -> DataChannelState {
    match value {
        0 => DataChannelState::Connecting,
        1 => DataChannelState::Open,
        2 => DataChannelState::Closing,
        3 => DataChannelState::Closed,
        _ => unreachable!("native adapter returned an invalid data-channel state"),
    }
}

fn priority(value: u16) -> DataChannelPriority {
    match value {
        128 => DataChannelPriority::VeryLow,
        256 => DataChannelPriority::Low,
        512 => DataChannelPriority::Medium,
        1024 => DataChannelPriority::High,
        value => DataChannelPriority::Custom(value),
    }
}
