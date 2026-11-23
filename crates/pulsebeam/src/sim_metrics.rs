#![allow(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::string_slice,
    clippy::indexing_slicing
)] // test / simulation support
//! Internal diagnostics and fault controls for deterministic simulation.
//!
//! Adaptation and forwarding samples are diagnostic, not correctness oracles. Application-facing
//! tests use public SDK observations and native decoded output. Fault-consumption evidence
//! qualifies injected input, independently of SUT correctness.
//!
//! Thread-local storage isolates parallel simulations driven on separate threads.
//! Compiled only under the `sim` feature.

pub mod wan_repair;

use pulsebeam_runtime::{sync::oneshot, time::Instant};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use crate::entity::ParticipantId;

/// Downstream bandwidth estimate observations, since the last [`reset`].
#[derive(Debug, Default, Clone)]
struct Samples {
    /// SFU media payload bytes per subscriber, excluding transport-generated overhead.
    forwarded_media_bytes: HashMap<ParticipantId, u64>,
    /// Last forwarding rank per publisher, aggregated across subscribers for diagnostics.
    /// Zero is paused; other ranks use [`pulsebeam_core::simulcast::LayerQuality`].
    forwarded_height: HashMap<ParticipantId, u32>,
    /// How many times each origin's forwarded layer changed since the last reset.
    ///
    /// Switching layer is not free: the receiver needs a keyframe and the picture stutters. A
    /// stream that changes several times a second never settles into a decodable run at all, so
    /// the viewer sees nothing rather than something imperfect - which is how this presents in
    /// production, and why counting the changes catches it where checking the final layer does
    /// not. A settled stream changes a handful of times over a minute.
    quality_changes: HashMap<ParticipantId, u64>,
    /// How many times each origin's forwarded layer *reversed direction* since the last reset.
    ///
    /// Climbing q -> h -> f as bandwidth is discovered is correct behaviour, so counting raw
    /// transitions cannot tell a healthy ramp from a stream oscillating between two layers. A
    /// reversal identifies oscillation when budget and intent are stable. Budget-driven
    /// suspension and resumption can legitimately reverse an optional stream's direction.
    quality_reversals: HashMap<ParticipantId, u64>,
    routing_counters: HashMap<String, u64>,
    fabric_udp_submission_peak: usize,
    wan_opus_wire_peak: usize,
    wan_live_data: HashMap<(IpAddr, IpAddr), (u64, u64)>,
    wan_data_loss: Option<WanDataLoss>,
    shard_data: Option<ShardDataTrace>,
    wan_data_sends: HashMap<String, pulsebeam_link::BindingCutoff>,
    /// Direction of each origin's last forwarded-layer change: `true` for an upgrade.
    last_quality_direction: HashMap<ParticipantId, bool>,
    /// Every downstream estimate as `(elapsed, estimate_bps, desired_bps)`, keyed by the
    /// *subscriber's* participant id.
    ///
    /// Min/max/last collapse a trace into three numbers, which cannot distinguish an estimate
    /// that settled at capacity from one that touched it once and fell away, nor an estimate
    /// that ramped in two seconds from one that took thirty. Both distinctions are the
    /// difference between working and broken congestion control, so the whole series is kept.
    ///
    /// Keyed per subscriber because capacity is configured per link: an assertion comparing an
    /// estimate against its link's capacity is meaningless if two subscribers' traces are mixed.
    ///
    /// Demand is kept alongside the estimate because most interesting claims are relative to it.
    /// An estimate far below link capacity is correct when the application is only asking for a
    /// fraction of it - a delay-based estimator cannot measure bandwidth it is not using - so
    /// "tracks capacity" is only a fair test under saturation. "Reached the lesser of what was
    /// wanted and what the link had" is the claim that holds in both regimes.
    bwe_series: HashMap<ParticipantId, Vec<(Duration, u64, u64)>>,
    /// Reference point for series timestamps, set on the first sample after a reset.
    series_origin: Option<Instant>,
}

// Fault requests cross hosts; their deadline uses the process-wide shimmed clock.
struct ControllerStall {
    until: std::time::Instant,
    begun: oneshot::Sender<()>,
}

thread_local! {
    static SAMPLES: RefCell<Samples> = RefCell::new(Samples::default());
    static CONTROLLER_STALL: RefCell<Option<ControllerStall>> = const { RefCell::new(None) };
    static CONTROLLER_STALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static STALLED_NATIVE_INPUTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static FAIL_NEXT_MATERIALIZATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn fail_next_materialization() {
    FAIL_NEXT_MATERIALIZATION.with(|fail| fail.set(true));
}

pub fn take_materialization_failure() -> bool {
    FAIL_NEXT_MATERIALIZATION.with(std::cell::Cell::take)
}

pub fn request_controller_stall(duration: Duration) -> (std::time::Instant, oneshot::Receiver<()>) {
    STALLED_NATIVE_INPUTS.with(|count| count.set(0));
    let until = std::time::Instant::now()
        .checked_add(duration)
        .unwrap_or_else(std::time::Instant::now);
    let (begun, receipt) = oneshot::channel();
    CONTROLLER_STALL.with_borrow_mut(|pending| {
        assert!(pending.replace(ControllerStall { until, begun }).is_none());
    });
    (until, receipt)
}

pub async fn wait_controller_stall() {
    let request = CONTROLLER_STALL.with_borrow_mut(Option::take);
    if let Some(ControllerStall { until, begun }) = request {
        CONTROLLER_STALLED.with(|stalled| stalled.set(true));
        let _ = begun.send(());
        pulsebeam_runtime::time::sleep(until.saturating_duration_since(std::time::Instant::now()))
            .await;
        CONTROLLER_STALLED.with(|stalled| stalled.set(false));
    }
}

pub(crate) fn record_stalled_native_input() {
    if CONTROLLER_STALLED.with(std::cell::Cell::get) {
        STALLED_NATIVE_INPUTS.with(|count| count.set(count.get().saturating_add(1)));
    }
}

pub fn stalled_native_inputs() -> u64 {
    STALLED_NATIVE_INPUTS.with(std::cell::Cell::get)
}

/// Record one downstream allocation pass. Called from the allocator's reporting path.
///
/// `subscriber` is the participant whose downstream link this estimate describes.
pub fn record_downstream_bwe_for(subscriber: ParticipantId, bwe_bps: u64, desired_bps: u64) {
    let now = Instant::now();
    SAMPLES.with_borrow_mut(|s| {
        let origin = *s.series_origin.get_or_insert(now);
        s.bwe_series.entry(subscriber).or_default().push((
            now.saturating_duration_since(origin),
            bwe_bps,
            desired_bps,
        ));
    });
}

#[derive(Clone, Debug, Default)]
pub struct WanDataLossReport {
    pub dropped: std::collections::BTreeSet<u64>,
    pub repaired: std::collections::BTreeSet<u64>,
    receipt_acks: std::collections::BTreeSet<(u64, u64)>,
}

type RawDataRecords = std::collections::BTreeMap<(u64, u64, u16), Box<[u8]>>;

#[derive(Clone, Debug)]
struct WanDataLoss {
    edge: (IpAddr, IpAddr),
    report: WanDataLossReport,
    ingress: RawDataRecords,
    egress: RawDataRecords,
    conflicting_records: bool,
}

pub(crate) fn record_wan_data_send(publication: String, cutoff: pulsebeam_link::BindingCutoff) {
    SAMPLES.with_borrow_mut(|samples| {
        samples.wan_data_sends.insert(publication, cutoff);
    });
}

pub fn wan_data_send_ready(publication: &str) -> bool {
    SAMPLES.with_borrow(|samples| {
        samples
            .wan_data_sends
            .get(publication)
            .is_some_and(pulsebeam_link::BindingCutoff::is_live)
    })
}

pub fn arm_wan_data_loss(from: IpAddr, to: IpAddr) {
    SAMPLES.with_borrow_mut(|samples| {
        samples.wan_data_loss = Some(WanDataLoss {
            edge: (from, to),
            report: WanDataLossReport::default(),
            ingress: RawDataRecords::new(),
            egress: RawDataRecords::new(),
            conflicting_records: false,
        });
    });
}

pub(crate) fn record_wan_data_sctp(
    source: u64,
    unit: u64,
    index: u16,
    bytes: &[u8],
    ingress: bool,
) {
    SAMPLES.with_borrow_mut(|samples| {
        let Some(loss) = samples.wan_data_loss.as_mut() else {
            return;
        };
        let records = if ingress {
            &mut loss.ingress
        } else {
            &mut loss.egress
        };
        let key = (source, unit, index);
        if let Some(existing) = records.get(&key) {
            loss.conflicting_records |= existing.as_ref() != bytes;
        } else if records.len() < usize::from(pulsebeam_proto::data_record::MAX_FRAGMENTS) {
            records.insert(key, bytes.into());
        }
    });
}

pub fn wan_data_raw_records_match(source: u64, unit: u64, count: u16) -> bool {
    SAMPLES.with_borrow(|samples| {
        samples.wan_data_loss.as_ref().is_some_and(|loss| {
            !loss.conflicting_records
                && (0..count).all(|index| {
                    let key = (source, unit, index);
                    loss.ingress
                        .get(&key)
                        .is_some_and(|input| loss.egress.get(&key) == Some(input))
                })
        })
    })
}

pub(crate) fn record_wan_data_receipt_ack(source: u64, unit: u64) {
    SAMPLES.with_borrow_mut(|samples| {
        if let Some(loss) = samples.wan_data_loss.as_mut() {
            loss.report.receipt_acks.insert((source, unit));
        }
    });
}

pub fn wan_data_receipt_ack(source: u64, unit: u64) -> bool {
    SAMPLES.with_borrow(|samples| {
        samples
            .wan_data_loss
            .as_ref()
            .is_some_and(|loss| loss.report.receipt_acks.contains(&(source, unit)))
    })
}

pub fn take_wan_data_loss() -> Option<WanDataLossReport> {
    SAMPLES.with_borrow_mut(|samples| samples.wan_data_loss.take().map(|loss| loss.report))
}

pub(crate) fn wan_data_loss_armed(edge: (IpAddr, IpAddr)) -> bool {
    SAMPLES.with_borrow(|samples| {
        samples
            .wan_data_loss
            .as_ref()
            .is_some_and(|loss| loss.edge == edge)
    })
}

pub(crate) fn drop_wan_live_data(edge: (IpAddr, IpAddr), position: u64) -> bool {
    SAMPLES.with_borrow_mut(|samples| {
        let Some(loss) = samples
            .wan_data_loss
            .as_mut()
            .filter(|loss| loss.edge == edge)
        else {
            return false;
        };
        loss.report.dropped.insert(position);
        true
    })
}

pub(crate) fn record_wan_data_repair(edge: (IpAddr, IpAddr), position: u64) {
    SAMPLES.with_borrow_mut(|samples| {
        if let Some(loss) = samples
            .wan_data_loss
            .as_mut()
            .filter(|loss| loss.edge == edge)
        {
            loss.report.repaired.insert(position);
        }
    });
}

pub(crate) fn record_wan_live_data(from: IpAddr, to: IpAddr, position: u64) {
    SAMPLES.with_borrow_mut(|samples| {
        let entry = samples.wan_live_data.entry((from, to)).or_default();
        *entry = (entry.0.saturating_add(1), position);
    });
}

/// Count and newest observed position on one authenticated WAN ingress.
pub fn wan_live_data(from: IpAddr, to: IpAddr) -> (u64, u64) {
    SAMPLES.with_borrow(|samples| {
        samples
            .wan_live_data
            .get(&(from, to))
            .copied()
            .unwrap_or_default()
    })
}

type ShardFragment = pulsebeam_proto::data_record::FragmentIdentity;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ShardDataFault {
    #[default]
    None,
    FirstLiveOffer,
    FirstReceipt,
    BindingCutoff,
    ReceiptExpiry,
    LateJoin,
}

#[derive(Debug, Default, Clone)]
struct HeldShardData {
    initial_notice_seen: bool,
    notice: Option<crate::route::fabric::ShardDataExpectation>,
    repair: Option<crate::route::fabric::ShardDataRepairImage>,
    release: bool,
    original_receivers: Box<[crate::route::fabric::ShardDataReceiver]>,
}

#[derive(Debug, Default, Clone)]
pub struct ShardDataTrace {
    fault: ShardDataFault,
    receipt: Option<(crate::route::fabric::ShardDataReceipt, Instant)>,
    notice: Option<(crate::route::fabric::ShardBinding, u64, Instant)>,
    held: HeldShardData,
    pub replayed_before_cutoff_deadline: bool,
    pub stale_writes: usize,
    pub receipt_retried_within_deadline: bool,
    pub late_receipt_rejected: bool,
    pub expired_unit_reclaimed: bool,
    pub accepted_receipt: Option<(u64, u64, u64)>,
    pub live: Vec<ShardFragment>,
    pub dropped: Option<ShardFragment>,
    pub repaired: Vec<ShardFragment>,
}

pub fn arm_shard_data_trace(fault: ShardDataFault) {
    SAMPLES.with_borrow_mut(|s| {
        s.shard_data = Some(ShardDataTrace {
            fault,
            ..ShardDataTrace::default()
        });
    });
}

pub fn take_shard_data_trace() -> Option<ShardDataTrace> {
    SAMPLES.with_borrow_mut(|s| s.shard_data.take())
}

pub(crate) fn drop_shard_data_offer(record: &[u8]) -> bool {
    SAMPLES.with_borrow_mut(|s| {
        let Some(trace) = &mut s.shard_data else {
            return false;
        };
        let Ok(fragment) = pulsebeam_proto::data_record::wire_identity(record) else {
            return false;
        };
        trace.live.push(fragment);
        if matches!(
            trace.fault,
            ShardDataFault::FirstLiveOffer
                | ShardDataFault::BindingCutoff
                | ShardDataFault::LateJoin
        ) && trace.dropped.is_none()
        {
            trace.dropped = Some(fragment);
            true
        } else {
            false
        }
    })
}

pub(crate) fn record_shard_data_repair(fragment: ShardFragment) {
    SAMPLES.with_borrow_mut(|s| {
        if let Some(trace) = &mut s.shard_data {
            trace.repaired.push(fragment);
        }
    });
}

pub(crate) fn record_shard_data_notice(notice: &crate::route::fabric::ShardDataExpectation) {
    SAMPLES.with_borrow_mut(|s| {
        if let Some(trace) = &mut s.shard_data
            && matches!(
                trace.fault,
                ShardDataFault::FirstReceipt
                    | ShardDataFault::BindingCutoff
                    | ShardDataFault::ReceiptExpiry
                    | ShardDataFault::LateJoin
            )
            && trace.notice.is_none()
        {
            trace.notice = Some((notice.binding, notice.unit.unit_seq, notice.expires));
        }
    });
}

pub(crate) fn drop_shard_data_receipt(
    receipt: crate::route::fabric::ShardDataReceipt,
    now: Instant,
) -> bool {
    SAMPLES.with_borrow_mut(|s| {
        let Some(trace) = s.shard_data.as_mut().filter(|trace| {
            matches!(
                trace.fault,
                ShardDataFault::FirstReceipt | ShardDataFault::ReceiptExpiry
            )
        }) else {
            return false;
        };
        if !trace.notice.is_some_and(|(binding, unit, _)| {
            binding == receipt.binding && unit == receipt.unit.unit_seq
        }) || (trace.fault == ShardDataFault::FirstReceipt && trace.receipt.is_some())
        {
            return false;
        }
        trace.receipt.get_or_insert((receipt, now));
        true
    })
}

pub(crate) fn record_shard_data_receipt(
    receipt: crate::route::fabric::ShardDataReceipt,
    now: Instant,
) {
    SAMPLES.with_borrow_mut(|s| {
        if let Some(trace) = &mut s.shard_data {
            trace.accepted_receipt = Some((
                receipt.binding.incarnation.get(),
                receipt.unit.unit_seq,
                receipt.branch.get(),
            ));
            trace.receipt_retried_within_deadline =
                trace.receipt.is_some_and(|(dropped, first)| {
                    receipt == dropped
                        && first < now
                        && trace.notice.is_some_and(|(_, _, end)| now < end)
                });
        }
    });
}

pub fn shard_data_expired() -> bool {
    SAMPLES.with_borrow(|s| {
        s.shard_data.as_ref().is_some_and(|trace| {
            trace.receipt.is_some()
                && trace
                    .notice
                    .is_some_and(|(_, _, deadline)| Instant::now() >= deadline)
        })
    })
}

pub(crate) fn take_expired_shard_data_receipt(
    frame: &crate::shard::worker::ShardFrame,
    now: Instant,
) -> Option<crate::shard::worker::ShardFrame> {
    use crate::shard::worker::ShardFrame;
    let ShardFrame::FabricDataReceipt(fresh) = frame else {
        return None;
    };
    SAMPLES.with_borrow(|s| {
        let trace = s.shard_data.as_ref().filter(|trace| {
            trace.fault == ShardDataFault::ReceiptExpiry && !trace.late_receipt_rejected
        })?;
        let (binding, unit, deadline) = trace.notice?;
        let (receipt, _) = trace.receipt?;
        (fresh.binding == binding && fresh.unit.unit_seq != unit && now >= deadline)
            .then_some(ShardFrame::FabricDataReceipt(receipt))
    })
}

pub(crate) fn record_rejected_shard_data_receipt(
    receipt: crate::route::fabric::ShardDataReceipt,
    now: Instant,
) {
    SAMPLES.with_borrow_mut(|s| {
        if let Some(trace) = s.shard_data.as_mut()
            && trace.fault == ShardDataFault::ReceiptExpiry
            && trace.receipt.is_some_and(|(held, _)| held == receipt)
            && trace.notice.is_some_and(|(_, _, deadline)| now >= deadline)
        {
            trace.late_receipt_rejected = true;
        }
    });
}

pub(crate) fn record_expired_shard_data_reclaimed(unit_seq: u64) {
    SAMPLES.with_borrow_mut(|s| {
        if let Some(trace) = s.shard_data.as_mut()
            && trace.fault == ShardDataFault::ReceiptExpiry
            && trace.notice.is_some_and(|(_, unit, _)| unit == unit_seq)
        {
            trace.expired_unit_reclaimed = true;
        }
    });
}

pub fn shard_data_cutoff_held() -> bool {
    SAMPLES.with_borrow(|s| {
        s.shard_data
            .as_ref()
            .is_some_and(|trace| trace.held.notice.is_some() && trace.held.repair.is_some())
    })
}

pub fn release_shard_data_cutoff() {
    SAMPLES.with_borrow_mut(|s| {
        if let Some(trace) = &mut s.shard_data {
            trace.held.release = true;
        }
    });
}

pub(crate) fn delay_shard_data_frame(frame: &crate::shard::worker::ShardFrame) -> bool {
    use crate::shard::worker::ShardFrame;
    SAMPLES.with_borrow_mut(|s| {
        let Some(trace) = &mut s.shard_data else {
            return false;
        };
        let Some((binding, unit, _)) = trace.notice.filter(|_| {
            matches!(
                trace.fault,
                ShardDataFault::BindingCutoff | ShardDataFault::LateJoin
            )
        }) else {
            return false;
        };
        match frame {
            ShardFrame::FabricDataExpectation(notice)
                if notice.binding == binding && notice.unit.unit_seq == unit =>
            {
                if !std::mem::replace(&mut trace.held.initial_notice_seen, true) {
                    return false;
                }
                trace.held.notice.get_or_insert_with(|| notice.clone());
                true
            }
            ShardFrame::FabricDataRepairImage(image)
                if image.repair.binding == binding
                    && image.repair.request.fragment.unit_seq == unit =>
            {
                trace.held.repair.get_or_insert_with(|| image.clone());
                true
            }
            _ => false,
        }
    })
}

pub(crate) fn take_released_shard_data(
    frame: &crate::shard::worker::ShardFrame,
    now: Instant,
) -> Option<[crate::shard::worker::ShardFrame; 2]> {
    use crate::shard::worker::ShardFrame;
    let ShardFrame::FabricDataExpectation(fresh) = frame else {
        return None;
    };
    SAMPLES.with_borrow_mut(|s| {
        let trace = s.shard_data.as_mut()?;
        let (binding, unit, deadline) = trace.notice?;
        let held = &mut trace.held;
        if !held.release
            || fresh.binding.destination.shard() != binding.destination.shard()
            || fresh.unit.unit_seq == unit
            || held.notice.is_none()
            || held.repair.is_none()
        {
            return None;
        }
        let notice = held.notice.take()?;
        let repair = held.repair.take()?;
        let correct_cohort = match trace.fault {
            ShardDataFault::BindingCutoff => fresh
                .receivers
                .iter()
                .all(|receiver| !notice.receivers.contains(receiver)),
            ShardDataFault::LateJoin => {
                held.original_receivers.clone_from(&notice.receivers);
                fresh.binding == binding
                    && fresh.receivers.len() == notice.receivers.len().saturating_add(1)
                    && notice
                        .receivers
                        .iter()
                        .all(|receiver| fresh.receivers.contains(receiver))
            }
            _ => false,
        };
        trace.replayed_before_cutoff_deadline = now < deadline && correct_cohort;
        held.release = false;
        Some([
            ShardFrame::FabricDataExpectation(notice),
            ShardFrame::FabricDataRepairImage(repair),
        ])
    })
}

pub(crate) fn is_stale_shard_data(record: &[u8], receiver: crate::entity::ConnectionId) -> bool {
    SAMPLES.with_borrow(|s| {
        s.shard_data
            .as_ref()
            .filter(|trace| trace.held.release || trace.replayed_before_cutoff_deadline)
            .filter(|trace| {
                trace.fault != ShardDataFault::LateJoin
                    || !trace
                        .held
                        .original_receivers
                        .iter()
                        .any(|original| original.connection == receiver)
            })
            .and_then(|trace| trace.notice)
            .is_some_and(|(_, unit, _)| {
                pulsebeam_proto::data_record::wire_identity(record)
                    .is_ok_and(|id| id.unit_seq == unit)
            })
    })
}

pub(crate) fn record_stale_shard_data_write() {
    SAMPLES.with_borrow_mut(|s| {
        if let Some(trace) = &mut s.shard_data {
            trace.stale_writes = trace.stale_writes.saturating_add(1);
        }
    });
}

pub(crate) fn record_fabric_udp_submission(protected_bytes: usize) {
    SAMPLES.with_borrow_mut(|samples| {
        samples.fabric_udp_submission_peak =
            samples.fabric_udp_submission_peak.max(protected_bytes);
    });
}

pub fn fabric_udp_submission_peak() -> usize {
    SAMPLES.with_borrow(|samples| samples.fabric_udp_submission_peak)
}

pub(crate) fn record_wan_opus_wire_bytes(protected_bytes: usize) {
    SAMPLES.with_borrow_mut(|samples| {
        samples.wan_opus_wire_peak = samples.wan_opus_wire_peak.max(protected_bytes);
    });
}

pub fn wan_opus_wire_peak() -> usize {
    SAMPLES.with_borrow(|samples| samples.wan_opus_wire_peak)
}

pub fn record_routing_counter(name: &'static str) {
    SAMPLES.with_borrow_mut(|s| {
        *s.routing_counters.entry(name.to_owned()).or_default() += 1;
    });
}

pub fn record_routing_work(name: &'static str, amount: usize) {
    SAMPLES.with_borrow_mut(|s| {
        *s.routing_counters.entry(name.to_owned()).or_default() += amount as u64;
    });
}

pub fn record_routing_drop(lane: &'static str, stage: &'static str, origin: &'static str) {
    SAMPLES.with_borrow_mut(|s| {
        let key = format!("routing_drop:{lane}:{stage}:{origin}");
        *s.routing_counters.entry(key).or_default() += 1;
    });
}

pub fn routing_counter(name: &'static str) -> u64 {
    SAMPLES.with_borrow(|s| s.routing_counters.get(name).copied().unwrap_or(0))
}

/// Start a new diagnostic measurement window.
pub fn reset() {
    SAMPLES.with_borrow_mut(|s| *s = Samples::default());
}

/// Record diagnostic media payload bytes handed to str0m for forwarding.
pub fn record_forwarded_media_for(subscriber: ParticipantId, bytes: u64) {
    SAMPLES.with_borrow_mut(|s| {
        *s.forwarded_media_bytes.entry(subscriber).or_default() += bytes;
    });
}

/// Media payload forwarded since [`reset`].
pub fn forwarded_media_bytes(subscriber: &str) -> u64 {
    let Ok(subscriber) = subscriber.parse() else {
        return 0;
    };
    forwarded_media_bytes_for(subscriber)
}

pub fn forwarded_media_bytes_for(subscriber: ParticipantId) -> u64 {
    SAMPLES.with_borrow(|s| {
        s.forwarded_media_bytes
            .get(&subscriber)
            .copied()
            .unwrap_or(0)
    })
}

/// Record the selected encoding's measured height in pixels for `origin`'s track.
/// `None` means the slot is paused. Called on every allocation pass so the last
/// value reflects the current steady state.
pub fn record_forwarded_height_for(origin: ParticipantId, height: Option<u32>) {
    let height = height.unwrap_or(0);
    SAMPLES.with_borrow_mut(|s| {
        // Count transitions, not passes: this is called every pass, changed or not.
        if let Some(previous) = s.forwarded_height.get(&origin).copied()
            && previous != height
        {
            tracing::debug!(%origin, previous, height, "forwarded quality transition");
            *s.quality_changes.entry(origin).or_default() += 1;

            let up = height > previous;
            if let Some(was_up) = s.last_quality_direction.insert(origin, up)
                && was_up != up
            {
                *s.quality_reversals.entry(origin).or_default() += 1;
            }
        }
        s.forwarded_height.insert(origin, height);
    });
}

/// Last-seen selected encoding height for `origin` since [`reset`]. `None` if nothing was ever
/// recorded for that origin; `Some(0)` means paused or no measured height.
pub fn forwarded_height(origin: &str) -> Option<u32> {
    let origin = origin.parse().ok()?;
    forwarded_height_for(origin)
}

pub fn forwarded_height_for(origin: ParticipantId) -> Option<u32> {
    SAMPLES.with_borrow(|s| s.forwarded_height.get(&origin).copied())
}

/// How many times `origin`'s forwarded layer changed since [`reset`].
pub fn quality_changes(origin: &str) -> u64 {
    let Ok(origin) = origin.parse() else {
        return 0;
    };
    quality_changes_for(origin)
}

pub fn quality_changes_for(origin: ParticipantId) -> u64 {
    SAMPLES.with_borrow(|s| s.quality_changes.get(&origin).copied().unwrap_or(0))
}

/// How many times `origin`'s forwarded layer reversed direction since [`reset`].
///
/// A monotonic ramp reports zero however many layers it climbs through.
pub fn quality_reversals(origin: &str) -> u64 {
    let Ok(origin) = origin.parse() else {
        return 0;
    };
    quality_reversals_for(origin)
}

pub fn quality_reversals_for(origin: ParticipantId) -> u64 {
    SAMPLES.with_borrow(|s| s.quality_reversals.get(&origin).copied().unwrap_or(0))
}

/// Diagnostic estimate trace since [`reset`], as `(elapsed, estimate, desired)`.
/// Empty when no allocation pass recorded this subscriber in the window.
pub fn bwe_series(subscriber: &str) -> Vec<(Duration, u64, u64)> {
    let Ok(subscriber) = subscriber.parse() else {
        return Vec::new();
    };
    bwe_series_for(subscriber)
}

pub fn bwe_series_for(subscriber: ParticipantId) -> Vec<(Duration, u64, u64)> {
    SAMPLES.with_borrow(|s| s.bwe_series.get(&subscriber).cloned().unwrap_or_default())
}
