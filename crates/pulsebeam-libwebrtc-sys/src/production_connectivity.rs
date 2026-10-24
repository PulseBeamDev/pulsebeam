//! Captive connectivity resources. Only IDs and independent owned values leave
//! the production graph; lookups canonicalize actual native interface identity.
use super::*;
use std::{fmt, num::NonZeroUsize};

#[derive(Default)]
pub(super) struct ConnectivityResources {
    dtmf: HashMap<SessionDtmfSenderId, Resource<crate::DtmfSender>>,
    ice: HashMap<SessionIceTransportId, Resource<crate::IceTransport>>,
    sctp: HashMap<SessionSctpTransportId, Resource<crate::SctpTransport>>,
    dtls: HashMap<SessionDtlsTransportId, Resource<crate::DtlsTransport>>,
}
impl ConnectivityResources {
    pub(super) fn retire_dtmf_observers(&self, peer: SessionPeerId) {
        for value in self.dtmf.values().filter(|value| value.peer == peer) {
            value.value.unregister_observer();
        }
        // Transport observers remain live to observe native close. Their owning
        // wrappers synchronously unregister on the network role before drop.
    }
}

/// Actual callback/snapshot values. Its DTLS ID refers to the exact native
/// reference in that snapshot, not a later SCTP getter. Explicitly releasing an
/// ID invalidates that ID, including copies in already returned snapshots.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSctpTransportInformation {
    pub state: crate::SctpTransportState,
    pub dtls_transport: Option<SessionDtlsTransportId>,
    pub max_message_size: Option<f64>,
    pub max_channels: Option<i32>,
    native_string: String,
}
impl SessionSctpTransportInformation {
    pub fn native_string(&self) -> &str {
        &self.native_string
    }
}

#[derive(Debug)]
pub enum SessionDtmfError {
    Resource(PeerError),
    Input(crate::DtmfInsertError),
}
impl fmt::Display for SessionDtmfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resource(error) => error.fmt(f),
            Self::Input(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for SessionDtmfError {}

#[derive(Debug)]
pub enum SessionTransportError {
    Resource(PeerError),
    Observer(crate::TransportObserverError),
    InternalUnavailable(crate::IceInternalUnavailable),
    Operation(crate::IceOperationError),
    Packet(crate::IcePacketSendError),
    IceObserver(crate::IcePacketObserverError),
}
impl fmt::Display for SessionTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resource(error) => error.fmt(f),
            Self::Observer(error) => error.fmt(f),
            Self::InternalUnavailable(error) => error.fmt(f),
            Self::Operation(error) => error.fmt(f),
            Self::Packet(error) => error.fmt(f),
            Self::IceObserver(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for SessionTransportError {}

impl<R: SessionRuntime> Session<R> {
    fn insert_dtls(
        &mut self,
        peer: SessionPeerId,
        value: crate::DtlsTransport,
    ) -> Result<SessionDtlsTransportId, PeerError> {
        if let Some((&id, _)) = self
            .connectivity
            .dtls
            .iter()
            .find(|(_, old)| old.value.same_resource(&value))
        {
            return Ok(id);
        }
        let id = SessionDtlsTransportId(next_id()?);
        self.connectivity.dtls.insert(id, Resource { peer, value });
        Ok(id)
    }
    fn insert_sctp(
        &mut self,
        peer: SessionPeerId,
        value: crate::SctpTransport,
    ) -> Result<SessionSctpTransportId, PeerError> {
        if let Some((&id, _)) = self
            .connectivity
            .sctp
            .iter()
            .find(|(_, old)| old.value.same_resource(&value))
        {
            return Ok(id);
        }
        let id = SessionSctpTransportId(next_id()?);
        self.connectivity.sctp.insert(id, Resource { peer, value });
        Ok(id)
    }
    fn insert_ice(
        &mut self,
        peer: SessionPeerId,
        value: crate::IceTransport,
    ) -> Result<SessionIceTransportId, PeerError> {
        if let Some((&id, _)) = self
            .connectivity
            .ice
            .iter()
            .find(|(_, old)| old.value.same_resource(&value))
        {
            return Ok(id);
        }
        let id = SessionIceTransportId(next_id()?);
        self.connectivity.ice.insert(id, Resource { peer, value });
        Ok(id)
    }

    pub fn peer_sctp_transport(
        &mut self,
        peer: SessionPeerId,
    ) -> Result<Option<SessionSctpTransportId>, PeerError> {
        let value = self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .sctp_transport();
        value.map(|value| self.insert_sctp(peer, value)).transpose()
    }
    pub fn peer_dtls_transport_by_mid(
        &mut self,
        peer: SessionPeerId,
        mid: &[u8],
    ) -> Result<Option<SessionDtlsTransportId>, PeerError> {
        let value = self
            .peers
            .get(&peer)
            .ok_or_else(missing)?
            .value
            .dtls_transport_by_mid(mid);
        value.map(|value| self.insert_dtls(peer, value)).transpose()
    }
    pub fn sender_dtls_transport(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Option<SessionDtlsTransportId>, PeerError> {
        let resource = self.senders.get(&sender).ok_or_else(missing)?;
        let peer = resource.peer;
        let value = resource.value.dtls_transport();
        value.map(|value| self.insert_dtls(peer, value)).transpose()
    }
    pub fn receiver_dtls_transport(
        &mut self,
        receiver: SessionReceiverId,
    ) -> Result<Option<SessionDtlsTransportId>, PeerError> {
        let resource = self.receivers.get(&receiver).ok_or_else(missing)?;
        let peer = resource.peer;
        let value = resource.value.dtls_transport();
        value.map(|value| self.insert_dtls(peer, value)).transpose()
    }
    pub fn sctp_dtls_transport(
        &mut self,
        transport: SessionSctpTransportId,
    ) -> Result<Option<SessionDtlsTransportId>, PeerError> {
        let resource = self.connectivity.sctp.get(&transport).ok_or_else(missing)?;
        let peer = resource.peer;
        let value = resource.value.dtls_transport();
        value.map(|value| self.insert_dtls(peer, value)).transpose()
    }
    pub fn dtls_ice_transport(
        &mut self,
        transport: SessionDtlsTransportId,
    ) -> Result<Option<SessionIceTransportId>, PeerError> {
        let resource = self.connectivity.dtls.get(&transport).ok_or_else(missing)?;
        let peer = resource.peer;
        let value = resource.value.ice_transport();
        value.map(|value| self.insert_ice(peer, value)).transpose()
    }
    /// Independent native value/certificate copies, safe to move without the
    /// session. This never returns a sequence-bound transport facade.
    pub fn dtls_information(
        &mut self,
        transport: SessionDtlsTransportId,
    ) -> Result<crate::DtlsTransportInformation, PeerError> {
        Ok(self
            .connectivity
            .dtls
            .get(&transport)
            .ok_or_else(missing)?
            .value
            .information())
    }
    fn copy_sctp_information(
        &mut self,
        peer: SessionPeerId,
        value: crate::SctpTransportInformation,
    ) -> Result<SessionSctpTransportInformation, PeerError> {
        let dtls_transport = value
            .dtls_transport()
            .map(|value| self.insert_dtls(peer, value))
            .transpose()?;
        Ok(SessionSctpTransportInformation {
            state: value.state(),
            dtls_transport,
            max_message_size: value.max_message_size(),
            max_channels: value.max_channels(),
            native_string: value.native_string(),
        })
    }
    pub fn sctp_information(
        &mut self,
        transport: SessionSctpTransportId,
    ) -> Result<SessionSctpTransportInformation, PeerError> {
        let resource = self.connectivity.sctp.get(&transport).ok_or_else(missing)?;
        let peer = resource.peer;
        let value = resource.value.information();
        self.copy_sctp_information(peer, value)
    }
    pub fn register_dtls_observer(
        &mut self,
        transport: SessionDtlsTransportId,
        capacity: usize,
    ) -> Result<(), SessionTransportError> {
        self.connectivity
            .dtls
            .get(&transport)
            .ok_or_else(|| SessionTransportError::Resource(missing()))?
            .value
            .register_observer(capacity)
            .map_err(SessionTransportError::Observer)
    }
    pub fn unregister_dtls_observer(
        &mut self,
        transport: SessionDtlsTransportId,
    ) -> Result<(), PeerError> {
        self.connectivity
            .dtls
            .get(&transport)
            .ok_or_else(missing)?
            .value
            .unregister_observer();
        Ok(())
    }
    pub fn take_dtls_event(
        &mut self,
        transport: SessionDtlsTransportId,
    ) -> Result<Option<crate::DtlsTransportEvent>, PeerError> {
        Ok(self
            .connectivity
            .dtls
            .get(&transport)
            .ok_or_else(missing)?
            .value
            .take_event())
    }
    pub fn take_dtls_dropped_events(
        &mut self,
        transport: SessionDtlsTransportId,
    ) -> Result<u64, PeerError> {
        Ok(self
            .connectivity
            .dtls
            .get(&transport)
            .ok_or_else(missing)?
            .value
            .take_dropped_events())
    }
    pub fn register_sctp_observer(
        &mut self,
        transport: SessionSctpTransportId,
        capacity: usize,
    ) -> Result<(), SessionTransportError> {
        self.connectivity
            .sctp
            .get(&transport)
            .ok_or_else(|| SessionTransportError::Resource(missing()))?
            .value
            .register_observer(capacity)
            .map_err(SessionTransportError::Observer)
    }
    pub fn unregister_sctp_observer(
        &mut self,
        transport: SessionSctpTransportId,
    ) -> Result<(), PeerError> {
        self.connectivity
            .sctp
            .get(&transport)
            .ok_or_else(missing)?
            .value
            .unregister_observer();
        Ok(())
    }
    pub fn take_sctp_event(
        &mut self,
        transport: SessionSctpTransportId,
    ) -> Result<Option<SessionSctpTransportInformation>, PeerError> {
        let resource = self.connectivity.sctp.get(&transport).ok_or_else(missing)?;
        let peer = resource.peer;
        let value = resource.value.take_event();
        value
            .map(|value| self.copy_sctp_information(peer, value))
            .transpose()
    }
    pub fn take_sctp_dropped_events(
        &mut self,
        transport: SessionSctpTransportId,
    ) -> Result<u64, PeerError> {
        Ok(self
            .connectivity
            .sctp
            .get(&transport)
            .ok_or_else(missing)?
            .value
            .take_dropped_events())
    }
    fn ice_internal(
        &self,
        transport: SessionIceTransportId,
    ) -> Result<crate::IceTransportInternal, SessionTransportError> {
        Ok(self
            .connectivity
            .ice
            .get(&transport)
            .ok_or_else(|| SessionTransportError::Resource(missing()))?
            .value
            .internal_access())
    }
    pub fn register_ice_packet_observer(
        &mut self,
        t: SessionIceTransportId,
        capacity: usize,
    ) -> Result<(), SessionTransportError> {
        self.ice_internal(t)?
            .register_packet_observer(capacity)
            .map_err(SessionTransportError::IceObserver)
    }
    pub fn unregister_ice_packet_observer(
        &mut self,
        t: SessionIceTransportId,
    ) -> Result<(), SessionTransportError> {
        self.ice_internal(t)?
            .unregister_packet_observer()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn take_ice_packet_event(
        &mut self,
        t: SessionIceTransportId,
    ) -> Result<Option<crate::IcePacketEvent>, SessionTransportError> {
        Ok(self.ice_internal(t)?.take_packet_event())
    }
    pub fn take_dropped_ice_packet_events(
        &mut self,
        t: SessionIceTransportId,
    ) -> Result<u64, SessionTransportError> {
        Ok(self.ice_internal(t)?.take_dropped_packet_events())
    }
    pub fn ice_packet_state(
        &mut self,
        t: SessionIceTransportId,
    ) -> Result<crate::IcePacketState, SessionTransportError> {
        self.ice_internal(t)?
            .packet_state()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn send_ice_packet(
        &mut self,
        t: SessionIceTransportId,
        data: &[u8],
        options: &crate::IcePacketOptions,
        flags: i32,
    ) -> Result<crate::IceSocketOperationResult, SessionTransportError> {
        self.ice_internal(t)?
            .send_packet(data, options, flags)
            .map_err(SessionTransportError::Packet)
    }
    pub fn set_ice_option(
        &mut self,
        t: SessionIceTransportId,
        option: crate::IceSocketOption,
        value: i32,
    ) -> Result<crate::IceSocketOperationResult, SessionTransportError> {
        self.ice_internal(t)?
            .set_option(option, value)
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn ice_option(
        &mut self,
        t: SessionIceTransportId,
        option: crate::IceSocketOption,
    ) -> Result<Option<i32>, SessionTransportError> {
        self.ice_internal(t)?
            .get_option(option)
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn ice_network_route(
        &mut self,
        t: SessionIceTransportId,
    ) -> Result<Option<crate::IceNetworkRoute>, SessionTransportError> {
        self.ice_internal(t)?
            .network_route()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn ice_stats(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<Option<crate::IceTransportStats>, SessionTransportError> {
        self.ice_internal(transport)?
            .stats()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn ice_selected_candidate_pair(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<Option<crate::IceCandidatePair>, SessionTransportError> {
        self.ice_internal(transport)?
            .selected_candidate_pair()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn add_remote_ice_candidate(
        &mut self,
        transport: SessionIceTransportId,
        candidate: &crate::TransportCandidate,
    ) -> Result<(), SessionTransportError> {
        self.ice_internal(transport)?
            .add_remote_candidate(candidate)
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn remove_remote_ice_candidate(
        &mut self,
        transport: SessionIceTransportId,
        candidate: &crate::TransportCandidate,
    ) -> Result<(), SessionTransportError> {
        self.ice_internal(transport)?
            .remove_remote_candidate(candidate)
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn remove_all_remote_ice_candidates(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<(), SessionTransportError> {
        self.ice_internal(transport)?
            .remove_all_remote_candidates()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn ice_config(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<crate::IceConfig, SessionTransportError> {
        self.ice_internal(transport)?
            .config()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn set_ice_config(
        &mut self,
        transport: SessionIceTransportId,
        config: &crate::IceConfig,
    ) -> Result<(), SessionTransportError> {
        self.ice_internal(transport)?
            .set_config(config)
            .map_err(SessionTransportError::Operation)
    }
    pub fn set_ice_role(
        &mut self,
        transport: SessionIceTransportId,
        role: crate::IceTransportRole,
    ) -> Result<(), SessionTransportError> {
        self.ice_internal(transport)?
            .set_role(role)
            .map_err(SessionTransportError::Operation)
    }
    pub fn set_ice_parameters(
        &mut self,
        transport: SessionIceTransportId,
        parameters: &crate::IceParameters,
    ) -> Result<(), SessionTransportError> {
        self.ice_internal(transport)?
            .set_parameters(parameters)
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn set_remote_ice_parameters(
        &mut self,
        transport: SessionIceTransportId,
        parameters: &crate::IceParameters,
    ) -> Result<(), SessionTransportError> {
        self.ice_internal(transport)?
            .set_remote_parameters(parameters)
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn set_remote_ice_mode(
        &mut self,
        transport: SessionIceTransportId,
        mode: crate::IceMode,
    ) -> Result<(), SessionTransportError> {
        self.ice_internal(transport)?
            .set_remote_mode(mode)
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn maybe_start_ice_gathering(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<(), SessionTransportError> {
        self.ice_internal(transport)?
            .maybe_start_gathering()
            .map_err(SessionTransportError::InternalUnavailable)
    }

    pub fn ice_internal_state(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<crate::IceTransportInternalState, SessionTransportError> {
        self.ice_internal(transport)?
            .state()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn ice_transport_state(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<crate::IceTransportState, SessionTransportError> {
        self.ice_internal(transport)?
            .ice_transport_state()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn ice_component(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<i32, SessionTransportError> {
        self.ice_internal(transport)?
            .component()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn ice_role(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<crate::IceTransportRole, SessionTransportError> {
        self.ice_internal(transport)?
            .role()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn ice_gathering_state(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<crate::IceTransportGatheringState, SessionTransportError> {
        self.ice_internal(transport)?
            .gathering_state()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn ice_rtt_estimate_ms(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<Option<i32>, SessionTransportError> {
        self.ice_internal(transport)?
            .rtt_estimate_ms()
            .map_err(SessionTransportError::InternalUnavailable)
    }
    pub fn release_dtls_transport(
        &mut self,
        transport: SessionDtlsTransportId,
    ) -> Result<(), PeerError> {
        self.connectivity
            .dtls
            .remove(&transport)
            .ok_or_else(missing)?;
        Ok(())
    }
    pub fn release_sctp_transport(
        &mut self,
        transport: SessionSctpTransportId,
    ) -> Result<(), PeerError> {
        self.connectivity
            .sctp
            .remove(&transport)
            .ok_or_else(missing)?;
        Ok(())
    }
    pub fn release_ice_transport(
        &mut self,
        transport: SessionIceTransportId,
    ) -> Result<(), PeerError> {
        self.connectivity
            .ice
            .remove(&transport)
            .ok_or_else(missing)?;
        Ok(())
    }

    pub fn sender_dtmf(
        &mut self,
        sender: SessionSenderId,
    ) -> Result<Option<SessionDtmfSenderId>, PeerError> {
        let resource = self.senders.get(&sender).ok_or_else(missing)?;
        let peer = resource.peer;
        let Some(value) = resource.value.dtmf_sender() else {
            return Ok(None);
        };
        if let Some((&id, _)) = self
            .connectivity
            .dtmf
            .iter()
            .find(|(_, old)| old.value.same_resource(&value))
        {
            return Ok(Some(id));
        }
        let id = SessionDtmfSenderId(next_id()?);
        self.connectivity.dtmf.insert(id, Resource { peer, value });
        Ok(Some(id))
    }
    fn dtmf(&self, sender: SessionDtmfSenderId) -> Result<&crate::DtmfSender, PeerError> {
        Ok(&self
            .connectivity
            .dtmf
            .get(&sender)
            .ok_or_else(missing)?
            .value)
    }
    pub fn dtmf_can_insert_tones(
        &mut self,
        sender: SessionDtmfSenderId,
    ) -> Result<bool, PeerError> {
        Ok(self.dtmf(sender)?.can_insert_tones())
    }
    pub fn insert_dtmf_tones(
        &mut self,
        sender: SessionDtmfSenderId,
        tones: &str,
        duration_ms: i32,
        gap_ms: i32,
    ) -> Result<bool, SessionDtmfError> {
        self.dtmf(sender)
            .map_err(SessionDtmfError::Resource)?
            .insert_tones(tones, duration_ms, gap_ms)
            .map_err(SessionDtmfError::Input)
    }
    pub fn insert_dtmf_tones_with_comma_delay(
        &mut self,
        sender: SessionDtmfSenderId,
        tones: &str,
        duration_ms: i32,
        gap_ms: i32,
        comma_delay_ms: i32,
    ) -> Result<bool, SessionDtmfError> {
        self.dtmf(sender)
            .map_err(SessionDtmfError::Resource)?
            .insert_tones_with_comma_delay(tones, duration_ms, gap_ms, comma_delay_ms)
            .map_err(SessionDtmfError::Input)
    }
    pub fn dtmf_tones(&mut self, sender: SessionDtmfSenderId) -> Result<String, PeerError> {
        Ok(self.dtmf(sender)?.tones())
    }
    pub fn dtmf_duration_ms(&mut self, sender: SessionDtmfSenderId) -> Result<i32, PeerError> {
        Ok(self.dtmf(sender)?.duration_ms())
    }
    pub fn dtmf_inter_tone_gap_ms(
        &mut self,
        sender: SessionDtmfSenderId,
    ) -> Result<i32, PeerError> {
        Ok(self.dtmf(sender)?.inter_tone_gap_ms())
    }
    pub fn dtmf_comma_delay_ms(&mut self, sender: SessionDtmfSenderId) -> Result<i32, PeerError> {
        Ok(self.dtmf(sender)?.comma_delay_ms())
    }
    pub fn register_dtmf_observer(
        &mut self,
        sender: SessionDtmfSenderId,
        capacity: NonZeroUsize,
    ) -> Result<bool, PeerError> {
        Ok(self.dtmf(sender)?.register_observer(capacity))
    }
    pub fn unregister_dtmf_observer(
        &mut self,
        sender: SessionDtmfSenderId,
    ) -> Result<(), PeerError> {
        self.dtmf(sender)?.unregister_observer();
        Ok(())
    }
    pub fn take_dtmf_tone_change(
        &mut self,
        sender: SessionDtmfSenderId,
    ) -> Result<Option<crate::DtmfToneChange>, PeerError> {
        Ok(self.dtmf(sender)?.take_tone_change())
    }
    pub fn take_dropped_dtmf_tone_changes(
        &mut self,
        sender: SessionDtmfSenderId,
    ) -> Result<u64, PeerError> {
        Ok(self.dtmf(sender)?.take_dropped_tone_changes())
    }
    pub fn release_dtmf_sender(&mut self, sender: SessionDtmfSenderId) -> Result<(), PeerError> {
        self.connectivity.dtmf.remove(&sender).ok_or_else(missing)?;
        Ok(())
    }
}
