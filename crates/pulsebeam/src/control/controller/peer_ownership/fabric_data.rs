use std::{num::NonZeroU64, time::Duration};

use pulsebeam_core::fabric::{
    Profile,
    wan::{WINDOW_TUPLE_BYTES, Window},
};
use pulsebeam_link::NodeRouteKey;

use crate::route::fabric::{
    LiveNetworkChild,
    local::{DataExpectation, PropagationOutcome, RepairFragment, RepairRequest},
    network_repair::RepairChild,
};

const VERSION: u8 = 1;
const EXPECT: u8 = 3;
const OBSERVED: u8 = 4;
const FRAME_BYTES: usize = 26;
const MAX_LEASE_MS: u16 = 2_000;
const LOOKUP: u8 = 5;
const LOCATED: u8 = 6;
const CANCEL: u8 = 8;
const UNAVAILABLE: u8 = 9;
const REPAIR_BYTES: usize = 40;
const LOCATED_BYTES: usize = 48;
const WAN_READY: u8 = 11;
const WAN_READY_BYTES: usize = LOCATED_BYTES + WINDOW_TUPLE_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) struct Scope {
    pub(in crate::control) key: NodeRouteKey,
    pub(in crate::control) source: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) enum Action {
    Expect { lease_ms: u16 },
    Observed(PropagationOutcome),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) struct Frame {
    pub(in crate::control) scope: Scope,
    pub(in crate::control) unit: DataExpectation,
    pub(in crate::control) action: Action,
}

impl Frame {
    pub(in crate::control) fn lease(self) -> Option<Duration> {
        let Action::Expect { lease_ms } = self.action else {
            return None;
        };
        (1..=MAX_LEASE_MS)
            .contains(&lease_ms)
            .then(|| Duration::from_millis(u64::from(lease_ms)))
    }

    fn valid(self) -> bool {
        self.scope.key.to_bytes() != [0; 4]
            && (1..=pulsebeam_proto::data_record::MAX_FRAGMENTS).contains(&self.unit.fragment_count)
            && (!matches!(self.action, Action::Expect { .. }) || self.lease().is_some())
    }

    pub(in crate::control) fn encode(self) -> Option<Box<[u8]>> {
        if !self.valid() {
            return None;
        }
        let (tag, argument) = match self.action {
            Action::Expect { lease_ms } => (EXPECT, lease_ms),
            Action::Observed(outcome) => (
                OBSERVED,
                match outcome {
                    PropagationOutcome::Confirmed => 1,
                    PropagationOutcome::Failed => 2,
                    PropagationOutcome::Unknown => 3,
                },
            ),
        };
        let mut bytes = Vec::with_capacity(FRAME_BYTES);
        bytes.extend([VERSION, tag]);
        bytes.extend(self.scope.key.to_bytes());
        bytes.extend(self.scope.source.get().to_be_bytes());
        bytes.extend(self.unit.unit_seq.to_be_bytes());
        bytes.extend(self.unit.fragment_count.to_be_bytes());
        bytes.extend(argument.to_be_bytes());
        Some(bytes.into_boxed_slice())
    }

    pub(in crate::control) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != FRAME_BYTES || bytes.first().copied()? != VERSION {
            return None;
        }
        let word = |range: std::ops::Range<usize>| -> Option<u16> {
            Some(u16::from_be_bytes(bytes.get(range)?.try_into().ok()?))
        };
        let argument = word(24..26)?;
        let action = match *bytes.get(1)? {
            EXPECT => Action::Expect { lease_ms: argument },
            OBSERVED => Action::Observed(match argument {
                1 => PropagationOutcome::Confirmed,
                2 => PropagationOutcome::Failed,
                3 => PropagationOutcome::Unknown,
                _ => return None,
            }),
            _ => return None,
        };
        let frame = Self {
            scope: Scope {
                key: NodeRouteKey::from_bytes(bytes.get(2..6)?.try_into().ok()?),
                source: NonZeroU64::new(u64::from_be_bytes(bytes.get(6..14)?.try_into().ok()?))?,
            },
            unit: DataExpectation {
                unit_seq: u64::from_be_bytes(bytes.get(14..22)?.try_into().ok()?),
                fragment_count: word(22..24)?,
            },
            action,
        };
        frame.valid().then_some(frame)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) enum RepairAction {
    Lookup,
    Located { sequence: u64 },
    Cancel,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) struct RepairFrame {
    pub(in crate::control) scope: Scope,
    pub(in crate::control) branch: NonZeroU64,
    pub(in crate::control) request: RepairRequest,
    pub(in crate::control) action: RepairAction,
}

impl RepairFrame {
    pub(in crate::control) fn encode(self) -> Option<Box<[u8]>> {
        if !valid_repair_scope(self.scope, self.request) {
            return None;
        }
        let (tag, size) = match self.action {
            RepairAction::Lookup => (LOOKUP, REPAIR_BYTES),
            RepairAction::Located { .. } => (LOCATED, LOCATED_BYTES),
            RepairAction::Cancel => (CANCEL, REPAIR_BYTES),
            RepairAction::Unavailable => (UNAVAILABLE, REPAIR_BYTES),
        };
        let mut bytes = Vec::with_capacity(size);
        write_repair_prefix(&mut bytes, tag, self.scope, self.branch, self.request);
        if let RepairAction::Located { sequence } = self.action {
            bytes.extend(sequence.to_be_bytes());
        }
        Some(bytes.into_boxed_slice())
    }

    pub(in crate::control) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.first().copied()? != VERSION {
            return None;
        }
        let value = |range: std::ops::Range<usize>| -> Option<u64> {
            Some(u64::from_be_bytes(bytes.get(range)?.try_into().ok()?))
        };
        let action = match (*bytes.get(1)?, bytes.len()) {
            (LOOKUP, REPAIR_BYTES) => RepairAction::Lookup,
            (LOCATED, LOCATED_BYTES) => RepairAction::Located {
                sequence: value(40..48)?,
            },
            (CANCEL, REPAIR_BYTES) => RepairAction::Cancel,
            (UNAVAILABLE, REPAIR_BYTES) => RepairAction::Unavailable,
            _ => return None,
        };
        let (scope, branch, request) = read_repair_prefix(bytes)?;
        valid_repair_scope(scope, request).then_some(Self {
            scope,
            branch,
            request,
            action,
        })
    }
}

fn write_repair_prefix(
    bytes: &mut Vec<u8>,
    kind: u8,
    scope: Scope,
    branch: NonZeroU64,
    request: RepairRequest,
) {
    bytes.extend([VERSION, kind]);
    bytes.extend(scope.key.to_bytes());
    bytes.extend(scope.source.get().to_be_bytes());
    bytes.extend(branch.get().to_be_bytes());
    bytes.extend(request.fragment.unit_seq.to_be_bytes());
    bytes.extend(request.fragment.index.to_be_bytes());
    bytes.extend(request.id.get().to_be_bytes());
}

fn read_repair_prefix(bytes: &[u8]) -> Option<(Scope, NonZeroU64, RepairRequest)> {
    let value = |range: std::ops::Range<usize>| -> Option<u64> {
        Some(u64::from_be_bytes(bytes.get(range)?.try_into().ok()?))
    };
    Some((
        Scope {
            key: NodeRouteKey::from_bytes(bytes.get(2..6)?.try_into().ok()?),
            source: NonZeroU64::new(value(6..14)?)?,
        },
        NonZeroU64::new(value(14..22)?)?,
        RepairRequest {
            fragment: RepairFragment {
                unit_seq: value(22..30)?,
                index: u16::from_be_bytes(bytes.get(30..32)?.try_into().ok()?),
            },
            id: NonZeroU64::new(value(32..40)?)?,
        },
    ))
}

fn valid_repair_scope(scope: Scope, request: RepairRequest) -> bool {
    scope.key.to_bytes() != [0; 4]
        && request.fragment.index < pulsebeam_proto::data_record::MAX_FRAGMENTS
}

/// Ready for an exact outstanding DATA repair on a selected WAN association.
/// The controller also verifies the request, parent, emission and key epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) struct WanRepairReady {
    pub(in crate::control) scope: Scope,
    pub(in crate::control) branch: NonZeroU64,
    pub(in crate::control) request: RepairRequest,
    pub(in crate::control) sequence: u64,
    pub(in crate::control) window: Window,
}

impl WanRepairReady {
    pub(in crate::control) fn from_install(
        parent: LiveNetworkChild,
        child: RepairChild,
        acknowledged: Window,
    ) -> Option<Self> {
        if child.window != Some(acknowledged) || !child.matches_parent(parent) {
            return None;
        }
        let ready = Self {
            scope: Scope {
                key: parent.ingress.key,
                source: parent.incarnation,
            },
            branch: child.route.branch,
            request: child.demand.request,
            sequence: child.demand.sequence,
            window: acknowledged,
        };
        ready.valid().then_some(ready)
    }

    fn valid(self) -> bool {
        valid_repair_scope(self.scope, self.request)
            && self.scope.key.to_bytes() != [0x0a, 0x0b, 0x0c, 0x0d]
            && self.window.profile() == Profile::Data
            && self.window.ssrc().get().to_be_bytes() != self.scope.key.to_bytes()
            && self.window.contains(self.sequence)
    }

    pub(in crate::control) fn encode(self) -> Option<Box<[u8]>> {
        if !self.valid() {
            return None;
        }
        let mut bytes = Vec::with_capacity(WAN_READY_BYTES);
        write_repair_prefix(&mut bytes, WAN_READY, self.scope, self.branch, self.request);
        bytes.extend(self.sequence.to_be_bytes());
        bytes.extend(self.window.to_tuple());
        Some(bytes.into_boxed_slice())
    }

    pub(in crate::control) fn read_scope(bytes: &[u8]) -> Option<Scope> {
        if bytes.len() != WAN_READY_BYTES || bytes.get(..2)? != [VERSION, WAN_READY] {
            return None;
        }
        read_repair_prefix(bytes).map(|(scope, _, _)| scope)
    }

    pub(in crate::control) fn decode(bytes: &[u8], emission: u16) -> Option<Self> {
        if bytes.len() != WAN_READY_BYTES || bytes.get(..2)? != [VERSION, WAN_READY] {
            return None;
        }
        let (scope, branch, request) = read_repair_prefix(bytes)?;
        let sequence = u64::from_be_bytes(bytes.get(REPAIR_BYTES..LOCATED_BYTES)?.try_into().ok()?);
        let window =
            Window::from_tuple(bytes.get(LOCATED_BYTES..)?.try_into().ok()?, emission).ok()?;
        let ready = Self {
            scope,
            branch,
            request,
            sequence,
            window,
        };
        ready.valid().then_some(ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wan_ready_pins_the_exact_data_repair_scope_position_and_window() {
        const WIRE: [u8; 65] = [
            1, 11, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0,
            3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 1, 0xff, 0xff, 3, 1, 0, 4, 0, 0, 0, 0,
            0, 1, 0x80, 0, 1, 5, 6, 7, 9,
        ];
        let ready = WanRepairReady::decode(&WIRE, 0).unwrap();
        assert_eq!(ready.scope.key.to_bytes(), [5, 6, 7, 8]);
        assert_eq!(ready.scope.source.get(), 1);
        assert_eq!(ready.branch.get(), 2);
        assert_eq!(ready.request.id.get(), 4);
        assert_eq!(ready.request.fragment.unit_seq, 3);
        assert_eq!(ready.request.fragment.index, 0);
        assert_eq!(ready.sequence, 0x1ffff);
        assert_eq!(ready.window.start(), 0x18000);
        assert_eq!(ready.window.ssrc().get(), 0x05060709);
        assert_eq!(ready.encode().as_deref(), Some(&WIRE[..]));
        assert!(RepairFrame::decode(&WIRE).is_none());
        let mut legacy = [0; 52];
        legacy[..48].copy_from_slice(&WIRE[..48]);
        legacy[1] = 7;
        legacy[48..].copy_from_slice(&ready.window.ssrc().get().to_be_bytes());
        assert!(WanRepairReady::decode(&legacy, 0).is_none());
        assert!(RepairFrame::decode(&legacy).is_none());
        for (offset, byte) in [(47, 0), (59, 1), (51, 2), (60, 4), (64, 8)] {
            let mut bad = WIRE;
            bad[offset] = byte;
            if offset == 47 {
                bad[46] = 0;
            }
            assert!(WanRepairReady::decode(&bad, 0).is_none());
        }
        assert!(WanRepairReady::decode(&WIRE[..64], 0).is_none());
        assert!(WanRepairReady::decode(&WIRE, 1 << 4).is_none());
    }

    #[test]
    fn repair_control_requires_exact_framing() {
        let scope = Scope {
            key: NodeRouteKey::from_bytes([1, 7, 0, 5]),
            source: NonZeroU64::MIN,
        };
        let base = RepairFrame {
            scope,
            branch: NonZeroU64::MIN,
            request: RepairRequest {
                fragment: RepairFragment {
                    unit_seq: 0,
                    index: 63,
                },
                id: NonZeroU64::MIN,
            },
            action: RepairAction::Lookup,
        };
        for (action, length) in [
            (RepairAction::Lookup, REPAIR_BYTES),
            (RepairAction::Located { sequence: u64::MAX }, LOCATED_BYTES),
            (RepairAction::Cancel, REPAIR_BYTES),
            (RepairAction::Unavailable, REPAIR_BYTES),
        ] {
            let frame = RepairFrame { action, ..base };
            let bytes = frame.encode().unwrap();
            assert_eq!(bytes.len(), length);
            assert_eq!(RepairFrame::decode(&bytes), Some(frame));
            assert!(Frame::decode(&bytes).is_none());
            assert!(super::super::transfer::Frame::decode(&bytes).is_none());
            assert!(RepairFrame::decode(bytes.get(..length - 1).unwrap()).is_none());
        }
        let mut invalid = base.encode().unwrap();
        invalid.get_mut(32..40).unwrap().fill(0);
        assert!(RepairFrame::decode(&invalid).is_none());
    }

    #[test]
    fn peer_expectation_is_exact_and_never_decoded_as_ownership_transfer() {
        let scope = Scope {
            key: NodeRouteKey::from_bytes([1, 7, 0, 5]),
            source: NonZeroU64::new(9).unwrap(),
        };
        let frame = Frame {
            scope,
            unit: DataExpectation {
                unit_seq: 0,
                fragment_count: 64,
            },
            action: Action::Expect { lease_ms: 1_500 },
        };
        let bytes = frame.encode().unwrap();
        assert_eq!(Frame::decode(&bytes), Some(frame));
        assert!(super::super::transfer::Frame::decode(&bytes).is_none());
        assert_eq!(frame.lease(), Some(Duration::from_millis(1_500)));
        assert!(Frame::decode(bytes.get(..25).unwrap()).is_none());
        assert!(
            Frame {
                action: Action::Expect { lease_ms: 2_001 },
                ..frame
            }
            .encode()
            .is_none()
        );
        let reply = Frame {
            action: Action::Observed(PropagationOutcome::Confirmed),
            ..frame
        };
        assert_eq!(Frame::decode(&reply.encode().unwrap()), Some(reply));
        assert_eq!(reply.lease(), None);
    }
}
