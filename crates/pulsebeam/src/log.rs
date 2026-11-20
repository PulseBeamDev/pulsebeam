//! This module defines cross-cutting tracing targets.

use crate::entity::{ConnectionId, ProjectId, PublicationId, PublicationKey, TrackKind};
use pulsebeam_core::{auth::TokenId, identity::NodeId};
use pulsebeam_link::{NetworkId, NodeRouteKey};
use pulsebeam_proto::state::CloseReason;

const INCIDENT: &str = "pulsebeam::incident";

pub(crate) fn credential_event(
    admitted: bool,
    project: ProjectId,
    jti: TokenId,
    connection: ConnectionId,
) {
    let (event, message) = if admitted {
        ("connection_admitted", "connection admitted")
    } else {
        ("credential_renewed", "connection credential renewed")
    };
    tracing::info!(target: INCIDENT, event, %project, %jti, conn = %connection, message);
}

#[derive(Clone, Copy)]
pub(crate) enum ConnectionEnd {
    Closed,
    AuthorizationExpired,
    AuthorizationRevoked,
    Replaced(Option<ConnectionId>),
}

impl ConnectionEnd {
    pub(crate) fn native_close(self) -> Option<CloseReason> {
        match self {
            Self::Closed | Self::AuthorizationExpired => None,
            Self::AuthorizationRevoked => Some(CloseReason::AuthorizationRevoked),
            Self::Replaced(_) => Some(CloseReason::ParticipantReplaced),
        }
    }
}

pub(crate) fn connection_ended(project: ProjectId, connection: ConnectionId, end: ConnectionEnd) {
    let (reason, winner) = match end {
        ConnectionEnd::Closed => ("closed", None),
        ConnectionEnd::AuthorizationExpired => ("authorization_expired", None),
        ConnectionEnd::AuthorizationRevoked => ("authorization_revoked", None),
        ConnectionEnd::Replaced(winner) => ("participant_replaced", winner),
    };
    tracing::info!(target: INCIDENT, event = "connection_ended", %project, conn = %connection,
        reason, winner = winner.as_ref().map(tracing::field::display));
}

pub(crate) fn publication_event(
    project: ProjectId,
    connection: ConnectionId,
    publication: PublicationKey,
    retirement: Option<&'static str>,
) {
    let event = if retirement.is_some() {
        "publication_retired"
    } else {
        "publication_admitted"
    };
    tracing::info!(target: INCIDENT, event, %project, conn = %connection, "pub" = %publication.publication_id(),
        kind = kind_code(publication.kind()), reason = retirement);
}

pub(crate) fn subscription_event(
    (project, connection): (ProjectId, ConnectionId),
    owned: u64,
    kind: TrackKind,
    event: &'static str,
    publication: Option<PublicationId>,
    reason: Option<&'static str>,
) {
    tracing::info!(target: INCIDENT, event, %project, conn = %connection, owned, kind = kind_code(kind),
        "pub" = publication.as_ref().map(tracing::field::display), reason);
}

#[derive(Clone, Copy)]
pub(crate) struct RelayNode {
    pub(crate) node: NodeId,
    pub(crate) incarnation: u64,
}

impl RelayNode {
    pub(crate) fn ephemeral() -> Self {
        Self {
            node: NodeId::new(),
            incarnation: 1,
        }
    }

    pub(crate) fn unknown_label(self, link: NetworkId, label: NodeRouteKey) {
        tracing::info!(target: "pulsebeam::incident::unattributed", event = "relay_unknown_label",
            node = %self.node, incarnation = self.incarnation, epoch = link.epoch(),
            label = u32::from_be_bytes(label.to_bytes()));
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RelayEvidence {
    pub(crate) local: RelayNode,
    pub(crate) peer: Option<NodeId>,
    pub(crate) link: NetworkId,
    pub(crate) label: NodeRouteKey,
    pub(crate) project: ProjectId,
    pub(crate) publication: PublicationKey,
    pub(crate) reverse: bool,
}

impl RelayEvidence {
    pub(crate) fn record(self, event: &'static str, destinations: usize) {
        tracing::info!(target: INCIDENT, event, project = %self.project,
            "pub" = %self.publication.publication_id(), kind = kind_code(self.publication.kind()),
            node = %self.local.node, incarnation = self.local.incarnation,
            peer = self.peer.as_ref().map(tracing::field::display), epoch = self.link.epoch(),
            label = u32::from_be_bytes(self.label.to_bytes()), destinations,
            direction = if self.reverse { "reverse" } else { "forward" });
    }
}

pub(crate) const fn kind_code(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Audio => "audio",
        TrackKind::Video => "video",
        TrackKind::Data => "data",
    }
}

pub(crate) const TARGET_VIDEO: &str = "pulsebeam::x::video";
pub(crate) const TARGET_AUDIO: &str = "pulsebeam::x::audio";

#[derive(Clone, Copy)]
pub(crate) struct LogCtx {
    pub connection_id: ConnectionId,
}

macro_rules! plog {
    ($level:ident, $ctx:expr, target: $target:expr, $($rest:tt)*) => {
        ::tracing::$level!(
            target: $target,
            connection_id = %$ctx.connection_id,
            $($rest)*
        )
    };
    ($level:ident, $ctx:expr, $($rest:tt)*) => {
        ::tracing::$level!(
            connection_id = %$ctx.connection_id,
            $($rest)*
        )
    };
}

macro_rules! plog_error {
    ($ctx:expr, $($rest:tt)*) => { $crate::log::plog!(error, $ctx, $($rest)*) };
}
macro_rules! plog_warn {
    ($ctx:expr, $($rest:tt)*) => { $crate::log::plog!(warn, $ctx, $($rest)*) };
}
macro_rules! plog_info {
    ($ctx:expr, $($rest:tt)*) => { $crate::log::plog!(info, $ctx, $($rest)*) };
}
macro_rules! plog_debug {
    ($ctx:expr, $($rest:tt)*) => { $crate::log::plog!(debug, $ctx, $($rest)*) };
}
macro_rules! plog_trace {
    ($ctx:expr, $($rest:tt)*) => { $crate::log::plog!(trace, $ctx, $($rest)*) };
}

pub(crate) use {plog, plog_debug, plog_error, plog_info, plog_trace, plog_warn};
