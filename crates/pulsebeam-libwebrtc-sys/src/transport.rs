#[path = "ice.rs"]
mod ice;
pub use ice::{
    CandidateNetworkCostOutOfRange, Dscp, IceCandidatePair, IceCandidatePairChange,
    IceCandidatePairState, IceCandidateStats, IceConfig, IceConnectionInfo, IceMode,
    IceNetworkRoute, IceNominationMode, IceOperationError, IcePacketDecryption, IcePacketEvent,
    IcePacketInfo, IcePacketObserverError, IcePacketOptions, IcePacketSendError, IcePacketState,
    IcePacketTimeUpdate, IceParameters, IceReceivedPacket, IceRouteEndpoint, IceSentPacket,
    IceSocketOperationResult, IceSocketOption, IceStunStats, IceTransportStats,
    MAX_ICE_CANDIDATE_NETWORK_COST, PacketInfoProtocol, PacketType, TransportCandidate,
    TransportCandidateProperties, TransportCandidateType,
};

use std::{cell::Cell, fmt, rc::Rc};

use crate::{CertificateData, ffi, peer::PeerInner};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DtlsTransportState {
    New,
    Connecting,
    Connected,
    Closed,
    Failed,
    NumValues,
    Other(i32),
}
impl DtlsTransportState {
    fn to_native(self) -> i32 {
        match self {
            Self::New => 0,
            Self::Connecting => 1,
            Self::Connected => 2,
            Self::Closed => 3,
            Self::Failed => 4,
            Self::NumValues => 5,
            Self::Other(value) => value,
        }
    }
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::New,
            1 => Self::Connecting,
            2 => Self::Connected,
            3 => Self::Closed,
            4 => Self::Failed,
            5 => Self::NumValues,
            value => Self::Other(value),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DtlsTransportRole {
    Server,
    Client,
    Other(i32),
}
impl DtlsTransportRole {
    fn to_native(self) -> i32 {
        match self {
            Self::Server => 0,
            Self::Client => 1,
            Self::Other(value) => value,
        }
    }
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::Server,
            1 => Self::Client,
            value => Self::Other(value),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SctpTransportState {
    New,
    Connecting,
    Connected,
    Closed,
    NumValues,
    Other(i32),
}
impl SctpTransportState {
    pub fn native_string(self) -> String {
        ffi::sctp_state_string(self.to_native())
    }
    fn to_native(self) -> i32 {
        match self {
            Self::New => 0,
            Self::Connecting => 1,
            Self::Connected => 2,
            Self::Closed => 3,
            Self::NumValues => 4,
            Self::Other(value) => value,
        }
    }
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::New,
            1 => Self::Connecting,
            2 => Self::Connected,
            3 => Self::Closed,
            4 => Self::NumValues,
            value => Self::Other(value),
        }
    }
}

/// Owned native DTLS snapshot, including a native-owned copy of the certificate
/// chain. No getter refreshes the transport or synthesizes negotiated values.
pub struct DtlsTransportInformation {
    native: cxx::UniquePtr<ffi::NativeDtlsInformation>,
}
// SAFETY: this is an independent native value copy, not a transport facade.
// It owns its certificate-chain clone, whose existing owning Rust wrapper is
// Send + Sync. Shared methods read/copy only; assignments require &mut self.
unsafe impl Send for DtlsTransportInformation {}
unsafe impl Sync for DtlsTransportInformation {}

impl Default for DtlsTransportInformation {
    fn default() -> Self {
        Self::from_native(ffi::dtls_information_default())
    }
}
impl DtlsTransportInformation {
    /// Native state-only constructor, not a negotiated transport.
    pub fn new(state: DtlsTransportState) -> Self {
        Self::from_native(ffi::dtls_information_new(state.to_native()))
    }
    pub fn from_parts(
        state: DtlsTransportState,
        role: Option<DtlsTransportRole>,
        tls_version: Option<i32>,
        ssl_cipher_suite: Option<i32>,
        srtp_cipher_suite: Option<i32>,
        certificates: Option<crate::TlsCertificateChain>,
        ssl_group_id: Option<i32>,
    ) -> Self {
        let fields = Self::constructor_fields(
            state,
            role,
            tls_version,
            ssl_cipher_suite,
            srtp_cipher_suite,
            certificates.is_some(),
            ssl_group_id,
        );
        Self::from_native(ffi::dtls_information_from_parts(
            &fields,
            certificates
                .map(crate::TlsCertificateChain::into_native)
                .unwrap_or_else(cxx::UniquePtr::null),
        ))
    }
    #[deprecated(note = "Use from_parts with native role and group options")]
    pub fn from_legacy_parts(
        state: DtlsTransportState,
        tls_version: Option<i32>,
        ssl_cipher_suite: Option<i32>,
        srtp_cipher_suite: Option<i32>,
        certificates: Option<crate::TlsCertificateChain>,
    ) -> Self {
        let fields = Self::constructor_fields(
            state,
            None,
            tls_version,
            ssl_cipher_suite,
            srtp_cipher_suite,
            certificates.is_some(),
            None,
        );
        Self::from_native(ffi::dtls_information_from_legacy_parts(
            &fields,
            certificates
                .map(crate::TlsCertificateChain::into_native)
                .unwrap_or_else(cxx::UniquePtr::null),
        ))
    }
    fn constructor_fields(
        state: DtlsTransportState,
        role: Option<DtlsTransportRole>,
        tls_version: Option<i32>,
        ssl_cipher_suite: Option<i32>,
        srtp_cipher_suite: Option<i32>,
        has_remote_certificates: bool,
        ssl_group_id: Option<i32>,
    ) -> ffi::FfiDtlsInformation {
        ffi::FfiDtlsInformation {
            state: state.to_native(),
            has_role: role.is_some(),
            role: role.map(DtlsTransportRole::to_native).unwrap_or(0),
            has_tls_version: tls_version.is_some(),
            tls_version: tls_version.unwrap_or(0),
            has_ssl_cipher_suite: ssl_cipher_suite.is_some(),
            ssl_cipher_suite: ssl_cipher_suite.unwrap_or(0),
            has_srtp_cipher_suite: srtp_cipher_suite.is_some(),
            srtp_cipher_suite: srtp_cipher_suite.unwrap_or(0),
            has_ssl_group_id: ssl_group_id.is_some(),
            ssl_group_id: ssl_group_id.unwrap_or(0),
            has_remote_certificates,
        }
    }
    fn from_native(native: cxx::UniquePtr<ffi::NativeDtlsInformation>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    /// Consumes the source through the native move-assignment operator.
    pub fn move_assign(&mut self, source: Self) {
        ffi::dtls_information_move_assign(self.native.pin_mut(), source.native);
    }
    fn native(&self) -> &ffi::NativeDtlsInformation {
        self.native.as_ref().unwrap()
    }
    fn fields(&self) -> ffi::FfiDtlsInformation {
        ffi::dtls_information_fields(self.native())
    }
    pub fn state(&self) -> DtlsTransportState {
        DtlsTransportState::from_native(self.fields().state)
    }
    pub fn role(&self) -> Option<DtlsTransportRole> {
        let value = self.fields();
        value
            .has_role
            .then(|| DtlsTransportRole::from_native(value.role))
    }
    pub fn tls_version(&self) -> Option<i32> {
        let value = self.fields();
        value.has_tls_version.then_some(value.tls_version)
    }
    pub fn ssl_cipher_suite(&self) -> Option<i32> {
        let value = self.fields();
        value.has_ssl_cipher_suite.then_some(value.ssl_cipher_suite)
    }
    pub fn srtp_cipher_suite(&self) -> Option<i32> {
        let value = self.fields();
        value
            .has_srtp_cipher_suite
            .then_some(value.srtp_cipher_suite)
    }
    pub fn ssl_group_id(&self) -> Option<i32> {
        let value = self.fields();
        value.has_ssl_group_id.then_some(value.ssl_group_id)
    }
    /// Clone of the actual retained native chain. Native absence and a present
    /// empty chain remain distinct; members expose the existing TLS helper graph.
    pub fn remote_ssl_certificates(&self) -> Option<crate::TlsCertificateChain> {
        crate::TlsCertificateChain::from_native(ffi::dtls_information_certificate_chain(
            self.native(),
        ))
    }
    /// Copied exports, not independently retained native certificates.
    pub fn remote_certificate_data(&self) -> Option<Vec<CertificateData>> {
        if !self.fields().has_remote_certificates {
            return None;
        }
        Some(
            ffi::dtls_information_remote_certificates(self.native())
                .into_iter()
                .map(|value| CertificateData {
                    pem: value.pem,
                    der: value.der,
                    expiration_unix_seconds: (value.expiration_unix_seconds != -1)
                        .then_some(value.expiration_unix_seconds),
                    signature_digest_algorithm: value
                        .has_signature_digest_algorithm
                        .then_some(value.signature_digest_algorithm),
                })
                .collect(),
        )
    }
}
impl Clone for DtlsTransportInformation {
    fn clone(&self) -> Self {
        Self::from_native(ffi::dtls_information_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::dtls_information_assign(self.native.pin_mut(), source.native());
    }
}

/// Owns the exact native SCTP snapshot and its DTLS reference. Retains the peer
/// origin so retrieving the snapshot's DTLS handle uses the correct network thread.
pub struct SctpTransportInformation {
    native: cxx::UniquePtr<ffi::NativeSctpInformation>,
    peer: Option<Rc<PeerInner>>,
}
impl Default for SctpTransportInformation {
    fn default() -> Self {
        Self::from_native(ffi::sctp_information_default(), None)
    }
}
impl SctpTransportInformation {
    /// Native state-only constructor with its actual absent DTLS reference.
    pub fn new(state: SctpTransportState) -> Self {
        Self::from_native(ffi::sctp_information_new(state.to_native()), None)
    }
    pub fn from_parts(
        state: SctpTransportState,
        dtls: Option<&DtlsTransport>,
        max_message_size: Option<f64>,
        max_channels: Option<i32>,
    ) -> Self {
        let fields = ffi::FfiSctpInformation {
            state: state.to_native(),
            has_dtls_transport: dtls.is_some(),
            has_max_message_size: max_message_size.is_some(),
            max_message_size: max_message_size.unwrap_or(0.0),
            has_max_channels: max_channels.is_some(),
            max_channels: max_channels.unwrap_or(0),
        };
        match dtls {
            Some(dtls) => Self::from_native(
                ffi::sctp_information_with_dtls(&fields, dtls.native()),
                Some(dtls.0.peer.clone()),
            ),
            None => Self::from_native(ffi::sctp_information_without_dtls(&fields), None),
        }
    }
    pub fn native_string(&self) -> String {
        ffi::sctp_information_string(self.native())
    }
    fn from_native(
        native: cxx::UniquePtr<ffi::NativeSctpInformation>,
        peer: Option<Rc<PeerInner>>,
    ) -> Self {
        assert!(!native.is_null());
        Self { native, peer }
    }
    fn native(&self) -> &ffi::NativeSctpInformation {
        self.native.as_ref().unwrap()
    }
    pub fn state(&self) -> SctpTransportState {
        SctpTransportState::from_native(ffi::sctp_information_fields(self.native()).state)
    }
    pub fn max_message_size(&self) -> Option<f64> {
        let value = ffi::sctp_information_fields(self.native());
        value.has_max_message_size.then_some(value.max_message_size)
    }
    pub fn max_channels(&self) -> Option<i32> {
        let value = ffi::sctp_information_fields(self.native());
        value.has_max_channels.then_some(value.max_channels)
    }
    pub fn dtls_transport(&self) -> Option<DtlsTransport> {
        if !ffi::sctp_information_fields(self.native()).has_dtls_transport {
            return None;
        }
        let peer = self
            .peer
            .as_ref()
            .expect("native SCTP DTLS reference retains its peer origin");
        DtlsTransport::from_native(
            ffi::sctp_information_dtls(self.native(), peer.native()),
            peer.clone(),
        )
    }
}
impl Clone for SctpTransportInformation {
    fn clone(&self) -> Self {
        Self::from_native(
            ffi::sctp_information_clone(self.native()),
            self.peer.clone(),
        )
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::sctp_information_assign(self.native.pin_mut(), source.native());
        self.peer = source.peer.clone();
    }
}

/// Actual owned RTCError copy from the native DTLS OnError callback.
pub type TransportError = crate::RtcError;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportObserverError {
    ZeroCapacity,
    AlreadyRegistered,
    NativeRejected,
}
impl fmt::Display for TransportObserverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}
impl std::error::Error for TransportObserverError {}

pub enum DtlsTransportEvent {
    StateChange(DtlsTransportInformation),
    Error(TransportError),
}
pub(crate) struct DtlsInner {
    // Native drops before peer so observer retirement can use its network thread.
    native: cxx::UniquePtr<ffi::NativeDtlsTransport>,
    peer: Rc<PeerInner>,
    registered: Cell<bool>,
}
/// Sequence-bound retained DTLS transport. All aliases share one registration.
#[derive(Clone)]
pub struct DtlsTransport(pub(crate) Rc<DtlsInner>);
impl DtlsTransport {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeDtlsTransport>,
        peer: Rc<PeerInner>,
    ) -> Option<Self> {
        if native.is_null() {
            return None;
        }
        let id = ffi::dtls_transport_identity(native.as_ref().unwrap());
        let mut handles = peer.dtls_transports.borrow_mut();
        handles.retain(|_, weak| weak.strong_count() != 0);
        if let Some(inner) = handles.get(&id).and_then(std::rc::Weak::upgrade) {
            return Some(Self(inner));
        }
        let inner = Rc::new(DtlsInner {
            native,
            peer: peer.clone(),
            registered: Cell::new(false),
        });
        handles.insert(id, Rc::downgrade(&inner));
        Some(Self(inner))
    }
    fn native(&self) -> &ffi::NativeDtlsTransport {
        self.0.native.as_ref().unwrap()
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
    pub fn information(&self) -> DtlsTransportInformation {
        DtlsTransportInformation::from_native(ffi::dtls_transport_information(self.native()))
    }
    pub fn ice_transport(&self) -> Option<IceTransport> {
        let native = ffi::dtls_transport_ice(self.native());
        (!native.is_null()).then(|| {
            IceTransport(Rc::new(IceInner {
                native,
                _peer: self.0.peer.clone(),
            }))
        })
    }
    /// One bounded queue per native transport. Registration clears the prior
    /// epoch's queue/counter; no Rust user code runs on a native callback thread.
    pub fn register_observer(&self, capacity: usize) -> Result<(), TransportObserverError> {
        if capacity == 0 {
            return Err(TransportObserverError::ZeroCapacity);
        }
        if self.0.registered.get() {
            return Err(TransportObserverError::AlreadyRegistered);
        }
        if !ffi::dtls_transport_register_observer(self.native(), capacity) {
            return Err(TransportObserverError::NativeRejected);
        }
        self.0.registered.set(true);
        Ok(())
    }
    /// Stops future callbacks; already queued events remain available to polling.
    pub fn unregister_observer(&self) {
        ffi::dtls_transport_unregister_observer(self.native());
        self.0.registered.set(false);
    }
    pub fn take_event(&self) -> Option<DtlsTransportEvent> {
        let mut event = ffi::dtls_transport_take_event(self.native());
        if event.is_null() {
            return None;
        }
        let information = ffi::dtls_event_information(event.pin_mut());
        if !information.is_null() {
            return Some(DtlsTransportEvent::StateChange(
                DtlsTransportInformation::from_native(information),
            ));
        }
        Some(DtlsTransportEvent::Error(crate::RtcError::from_native(
            ffi::dtls_event_error(event.as_ref().unwrap()),
        )))
    }
    /// Oldest-event eviction count, saturating at u64::MAX, reset on reading.
    pub fn take_dropped_events(&self) -> u64 {
        ffi::dtls_transport_take_dropped(self.native())
    }
}

pub(crate) struct SctpInner {
    native: cxx::UniquePtr<ffi::NativeSctpTransport>,
    peer: Rc<PeerInner>,
    registered: Cell<bool>,
}
#[derive(Clone)]
pub struct SctpTransport(pub(crate) Rc<SctpInner>);
impl SctpTransport {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeSctpTransport>,
        peer: Rc<PeerInner>,
    ) -> Option<Self> {
        if native.is_null() {
            return None;
        }
        let id = ffi::sctp_transport_identity(native.as_ref().unwrap());
        let mut handles = peer.sctp_transports.borrow_mut();
        handles.retain(|_, weak| weak.strong_count() != 0);
        if let Some(inner) = handles.get(&id).and_then(std::rc::Weak::upgrade) {
            return Some(Self(inner));
        }
        let inner = Rc::new(SctpInner {
            native,
            peer: peer.clone(),
            registered: Cell::new(false),
        });
        handles.insert(id, Rc::downgrade(&inner));
        Some(Self(inner))
    }
    fn native(&self) -> &ffi::NativeSctpTransport {
        self.0.native.as_ref().unwrap()
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
    pub fn information(&self) -> SctpTransportInformation {
        SctpTransportInformation::from_native(
            ffi::sctp_transport_information(self.native()),
            Some(self.0.peer.clone()),
        )
    }
    pub fn dtls_transport(&self) -> Option<DtlsTransport> {
        DtlsTransport::from_native(ffi::sctp_transport_dtls(self.native()), self.0.peer.clone())
    }
    pub fn register_observer(&self, capacity: usize) -> Result<(), TransportObserverError> {
        if capacity == 0 {
            return Err(TransportObserverError::ZeroCapacity);
        }
        if self.0.registered.get() {
            return Err(TransportObserverError::AlreadyRegistered);
        }
        if !ffi::sctp_transport_register_observer(self.native(), capacity) {
            return Err(TransportObserverError::NativeRejected);
        }
        self.0.registered.set(true);
        Ok(())
    }
    pub fn unregister_observer(&self) {
        ffi::sctp_transport_unregister_observer(self.native());
        self.0.registered.set(false);
    }
    pub fn take_event(&self) -> Option<SctpTransportInformation> {
        let native = ffi::sctp_transport_take_event(self.native());
        (!native.is_null())
            .then(|| SctpTransportInformation::from_native(native, Some(self.0.peer.clone())))
    }
    pub fn take_dropped_events(&self) -> u64 {
        ffi::sctp_transport_take_dropped(self.native())
    }
}

struct IceInner {
    native: cxx::UniquePtr<ffi::NativeIceTransport>,
    _peer: Rc<PeerInner>,
}
/// Retains the native public ICE facade, not its revocable internal pointer.
#[derive(Clone)]
pub struct IceTransport(Rc<IceInner>);
impl IceTransport {
    pub(crate) fn internal_access(&self) -> IceTransportInternal {
        IceTransportInternal(self.clone())
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::ice_transport_same_resource(self.native(), other.native())
    }

    fn native(&self) -> &ffi::NativeIceTransport {
        self.0.native.as_ref().unwrap()
    }
    /// Returns an access facade, not a durable internal-native handle. Every
    /// operation reacquires internal() within one network-thread task; teardown
    /// may revoke it after this call and is reported by subsequent operations.
    pub fn internal(&self) -> Option<IceTransportInternal> {
        ffi::ice_transport_has_internal(self.native()).then(|| IceTransportInternal(self.clone()))
    }
}
/// Pinned internal connectivity state, distinct from the public ICE state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IceTransportInternalState {
    Init,
    Connecting,
    Completed,
    Failed,
    Other(i32),
}
impl IceTransportInternalState {
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::Init,
            1 => Self::Connecting,
            2 => Self::Completed,
            3 => Self::Failed,
            value => Self::Other(value),
        }
    }
}
/// Native public ICE state. Failed is not terminal at this pin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IceTransportState {
    New,
    Checking,
    Connected,
    Completed,
    Failed,
    Disconnected,
    Closed,
    Other(i32),
}
impl IceTransportState {
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::New,
            1 => Self::Checking,
            2 => Self::Connected,
            3 => Self::Completed,
            4 => Self::Failed,
            5 => Self::Disconnected,
            6 => Self::Closed,
            value => Self::Other(value),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IceTransportRole {
    Controlling,
    Controlled,
    Unknown,
    Other(i32),
}
impl IceTransportRole {
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::Controlling,
            1 => Self::Controlled,
            2 => Self::Unknown,
            value => Self::Other(value),
        }
    }
}
/// P2P transport gathering state, not a fabricated peer-level observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IceTransportGatheringState {
    New,
    Gathering,
    Complete,
    Other(i32),
}
impl IceTransportGatheringState {
    fn from_native(value: i32) -> Self {
        match value {
            0 => Self::New,
            1 => Self::Gathering,
            2 => Self::Complete,
            value => Self::Other(value),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IceInternalUnavailable;
impl fmt::Display for IceInternalUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("native ICE internal transport has been cleared")
    }
}
impl std::error::Error for IceInternalUnavailable {}
#[derive(Clone)]
pub struct IceTransportInternal(IceTransport);
impl IceTransportInternal {
    fn value(&self, field: u8) -> Result<ffi::FfiIceInternalValue, IceInternalUnavailable> {
        let value = ffi::ice_transport_internal_value(self.0.native(), field);
        if value.available {
            Ok(value)
        } else {
            Err(IceInternalUnavailable)
        }
    }
    pub fn state(&self) -> Result<IceTransportInternalState, IceInternalUnavailable> {
        Ok(IceTransportInternalState::from_native(self.value(0)?.value))
    }
    pub fn ice_transport_state(&self) -> Result<IceTransportState, IceInternalUnavailable> {
        Ok(IceTransportState::from_native(self.value(1)?.value))
    }
    pub fn component(&self) -> Result<i32, IceInternalUnavailable> {
        Ok(self.value(2)?.value)
    }
    pub fn role(&self) -> Result<IceTransportRole, IceInternalUnavailable> {
        Ok(IceTransportRole::from_native(self.value(3)?.value))
    }
    pub fn gathering_state(&self) -> Result<IceTransportGatheringState, IceInternalUnavailable> {
        Ok(IceTransportGatheringState::from_native(
            self.value(4)?.value,
        ))
    }
    pub fn rtt_estimate_ms(&self) -> Result<Option<i32>, IceInternalUnavailable> {
        let value = self.value(5)?;
        Ok(value.present.then_some(value.value))
    }
}
