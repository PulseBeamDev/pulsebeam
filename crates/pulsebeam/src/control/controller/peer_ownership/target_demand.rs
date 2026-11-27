use std::num::{NonZeroU32, NonZeroU64};

use pulsebeam_core::{
    dd::evidence::{ProjectionContext, TargetSet},
    fabric::wan::FEEDBACK_SSRC,
};
use pulsebeam_link::{LinkTime, PeerControl};
use pulsebeam_runtime::time::{Duration, Instant};

use crate::route::fabric::target_demand::{MAX_LEASE, Projection, Update};

// A separate control record, never an ownership claim or a repair request.
const TAG: [u8; 2] = [1, 0x19];
const SIZE: usize = 40;
const BUDGET_TAG: [u8; 2] = [1, 0x1a];
const BUDGET_SIZE: usize = 28;
const BUDGET_TTL_MS: u16 = 450;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::control) struct Budget {
    pub label: NonZeroU32,
    pub incarnation: NonZeroU64,
    pub generation: NonZeroU64,
    pub bps: u32,
    ttl_ms: u16,
}

impl Budget {
    fn encode(self) -> Box<[u8]> {
        let mut out = Vec::with_capacity(BUDGET_SIZE);
        out.extend(BUDGET_TAG);
        out.extend(self.label.get().to_be_bytes());
        out.extend(self.incarnation.get().to_be_bytes());
        out.extend(self.generation.get().to_be_bytes());
        out.extend(self.bps.to_be_bytes());
        out.extend(self.ttl_ms.to_be_bytes());
        out.into_boxed_slice()
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != BUDGET_SIZE || !bytes.starts_with(&BUDGET_TAG) {
            return None;
        }
        let label = NonZeroU32::new(u32::from_be_bytes(bytes.get(2..6)?.try_into().ok()?))?;
        let ttl_ms = u16::from_be_bytes(bytes.get(26..28)?.try_into().ok()?);
        if label.get() == FEEDBACK_SSRC || !(1..=BUDGET_TTL_MS).contains(&ttl_ms) {
            return None;
        }
        Some(Self {
            label,
            incarnation: NonZeroU64::new(u64::from_be_bytes(bytes.get(6..14)?.try_into().ok()?))?,
            generation: NonZeroU64::new(u64::from_be_bytes(bytes.get(14..22)?.try_into().ok()?))?,
            bps: u32::from_be_bytes(bytes.get(22..26)?.try_into().ok()?),
            ttl_ms,
        })
    }

    pub fn expires(self, now: Instant) -> Option<Instant> {
        now.checked_add(Duration::from_millis(u64::from(self.ttl_ms)))
    }

    pub(in crate::control::controller) fn reply(
        lane: &PeerControl,
        epoch: Instant,
        now: Instant,
        demand: Demand,
        bps: u32,
        expires: Instant,
    ) -> Option<()> {
        if !matches!(demand.projection, Some(Projection::Targets { .. })) {
            return None;
        }
        let ttl_ms = u16::try_from(
            expires
                .checked_duration_since(now)?
                .as_millis()
                .min(u128::from(BUDGET_TTL_MS)),
        )
        .ok()?;
        let send_before = now.checked_add(Duration::from_millis(60))?.min(expires);
        if ttl_ms <= 60 {
            return None;
        }
        let cutoff =
            LinkTime(u64::try_from(send_before.checked_duration_since(epoch)?.as_nanos()).ok()?);
        let budget = Self {
            label: demand.label,
            incarnation: demand.incarnation,
            generation: demand.generation,
            bps,
            ttl_ms: ttl_ms.checked_sub(60)?,
        };
        lane.try_send(budget.encode(), cutoff).ok()
    }
}

pub(in crate::control::controller) fn reply_with_budget(
    actor: &super::super::ControllerActor,
    network: pulsebeam_link::NetworkId,
    lane: &PeerControl,
    epoch: Instant,
    now: Instant,
    demand: Demand,
    route_expires: Instant,
) {
    if let Some((bps, expires)) = actor
        .links
        .as_ref()
        .and_then(|links| links.wan_video_share(network, now))
    {
        let _ = Budget::reply(lane, epoch, now, demand, bps, expires.min(route_expires));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::control::controller) struct Demand {
    pub label: NonZeroU32,
    pub incarnation: NonZeroU64,
    pub generation: NonZeroU64,
    // None is explicit withdrawal. Some(Whole) still requests the source.
    pub projection: Option<Projection>,
    pub ttl_ms: u16,
}

impl Demand {
    pub fn encode(self) -> Option<Box<[u8]>> {
        if self.label.get() == FEEDBACK_SSRC
            || self.projection.is_some() != (self.ttl_ms != 0)
            || Duration::from_millis(u64::from(self.ttl_ms)) > MAX_LEASE
        {
            return None;
        }
        let (attachment, active, mask) = match self.projection {
            Some(Projection::Targets { context, targets }) => {
                Projection::targets(context, targets)?;
                (context.attachment, context.active_targets, targets.mask())
            }
            _ => (0, 0, 0),
        };
        let mut bytes = Vec::with_capacity(SIZE);
        bytes.extend(TAG);
        bytes.extend(self.label.get().to_be_bytes());
        bytes.extend(self.incarnation.get().to_be_bytes());
        bytes.extend(self.generation.get().to_be_bytes());
        bytes.extend(self.ttl_ms.to_be_bytes());
        bytes.extend(attachment.to_be_bytes());
        bytes.extend(active.to_be_bytes());
        bytes.extend(mask.to_be_bytes());
        Some(bytes.into_boxed_slice())
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != SIZE || !bytes.starts_with(&TAG) {
            return None;
        }
        let label = NonZeroU32::new(u32::from_be_bytes(bytes.get(2..6)?.try_into().ok()?))?;
        let incarnation = NonZeroU64::new(u64::from_be_bytes(bytes.get(6..14)?.try_into().ok()?))?;
        let generation = NonZeroU64::new(u64::from_be_bytes(bytes.get(14..22)?.try_into().ok()?))?;
        let ttl_ms = u16::from_be_bytes(bytes.get(22..24)?.try_into().ok()?);
        if label.get() == FEEDBACK_SSRC || Duration::from_millis(u64::from(ttl_ms)) > MAX_LEASE {
            return None;
        }
        let attachment = u64::from_be_bytes(bytes.get(24..32)?.try_into().ok()?);
        let active_targets = u32::from_be_bytes(bytes.get(32..36)?.try_into().ok()?);
        let mask = u32::from_be_bytes(bytes.get(36..40)?.try_into().ok()?);
        let projection = match (ttl_ms, attachment, active_targets, mask) {
            (0, 0, 0, 0) => None,
            (0, ..) => return None,
            (_, 0, 0, 0) => Some(Projection::Whole),
            _ => Some(Projection::targets(
                ProjectionContext {
                    attachment,
                    active_targets,
                },
                TargetSet::new(mask)?,
            )?),
        };
        Some(Self {
            label,
            incarnation,
            generation,
            projection,
            ttl_ms,
        })
    }

    pub fn update(self, now: Instant, route_expires: Instant) -> Option<Update> {
        if now >= route_expires {
            return None;
        }
        Some(match self.projection {
            Some(projection) => Update::Lease {
                projection,
                expires: now
                    .checked_add(Duration::from_millis(u64::from(self.ttl_ms)))?
                    .min(route_expires),
            },
            None => Update::Withdraw,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_a_distinct_bounded_window_response() {
        let budget = Budget {
            label: NonZeroU32::MIN,
            incarnation: NonZeroU64::MIN,
            generation: NonZeroU64::MIN,
            bps: 420_000,
            ttl_ms: 200,
        };
        assert_eq!(Budget::decode(&budget.encode()), Some(budget));
        assert_eq!(Demand::decode(&budget.encode()), None);
        let invalid = Budget {
            ttl_ms: BUDGET_TTL_MS + 1,
            ..budget
        };
        assert_eq!(Budget::decode(&invalid.encode()), None);
    }

    #[test]
    fn distinguishes_withdrawal_whole_and_target_updates() {
        let withdraw = Demand {
            label: NonZeroU32::MIN,
            incarnation: NonZeroU64::MIN,
            generation: NonZeroU64::MIN,
            projection: None,
            ttl_ms: 0,
        };
        let whole = Demand {
            projection: Some(Projection::Whole),
            ttl_ms: 500,
            ..withdraw
        };
        let targets = Demand {
            projection: Projection::targets(
                ProjectionContext {
                    attachment: 0,
                    active_targets: 0b111,
                },
                TargetSet::new(0b101).unwrap(),
            ),
            ..whole
        };
        for update in [withdraw, whole, targets] {
            let wire = update.encode().unwrap();
            assert_eq!(Demand::decode(&wire), Some(update));
            assert_eq!(Demand::decode(&wire[1..]), None);
        }
        let mut invalid = targets.encode().unwrap();
        invalid[22..24].fill(0);
        assert_eq!(Demand::decode(&invalid), None);
        let mut invalid = targets.encode().unwrap();
        invalid[39] = 0b1001;
        assert_eq!(Demand::decode(&invalid), None);
        let now = Instant::now();
        let route_expires = now + Duration::from_millis(100);
        assert_eq!(
            whole.update(now, route_expires),
            Some(Update::Lease {
                projection: Projection::Whole,
                expires: route_expires
            })
        );
        assert_eq!(whole.update(route_expires, route_expires), None);
    }
}
