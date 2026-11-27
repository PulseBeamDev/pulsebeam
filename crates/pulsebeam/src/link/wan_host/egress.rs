use std::{fmt, num::NonZeroU64, task::Poll};

use pulsebeam_link::{BindError, BindingCutoff, BindingId};
use pulsebeam_runtime::sync::oneshot;

use super::*;

mod protection;
mod schedule;
use protection::Envelope;
use schedule::{Class, Schedule};

const BUDGET: usize = 32;

pub(super) type MediaSenders = [pulsebeam_queue::Sender<Media>; Class::ALL.len()];
pub(super) struct MediaInbox {
    lanes: [pulsebeam_queue::Receiver<Media>; Class::ALL.len()],
    budget: pulsebeam_runtime::sync::watch::Sender<Option<VideoBudget>>,
}

#[derive(Clone, Copy)]
pub(super) struct VideoBudget {
    pub share_bps: u32,
    pub expires: Instant,
}

pub(super) type BudgetReader = pulsebeam_runtime::sync::watch::Receiver<Option<VideoBudget>>;

pub(super) fn media_channels() -> (MediaSenders, MediaInbox, BudgetReader) {
    let [
        (audio, audio_rx),
        (protected, protected_rx),
        (live, live_rx),
        (repair, repair_rx),
    ] = Class::ALL.map(|class| {
        pulsebeam_queue::bounded(NonZeroUsize::new(class.capacity()).expect("bounded WAN class"))
    });
    let (budget, watch) = pulsebeam_runtime::sync::watch::channel(None);
    (
        [audio, protected, live, repair],
        MediaInbox {
            lanes: [audio_rx, protected_rx, live_rx, repair_rx],
            budget,
        },
        watch,
    )
}

fn media_class(window: Window, purpose: Purpose, protected: bool) -> Class {
    match purpose {
        Purpose::Repair { .. } => Class::Repair,
        Purpose::Live if window.profile() == pulsebeam_core::fabric::Profile::Opus => Class::Audio,
        Purpose::Live if protected => Class::Protected,
        Purpose::Live => Class::Live,
    }
}

#[derive(Default)]
pub(super) struct Egress {
    pub(super) inbox: Option<MediaInbox>,
    pub(super) protected_egress_bps: Option<NonZeroU64>,
    routes: BTreeMap<NonZeroU32, Active>,
    queued: Schedule<Media>,
    blocked: [Option<Blocked>; Class::ALL.len()],
    departures: Departures,
    budget_at: Option<Instant>,
}

#[derive(Default)]
struct Departures {
    report_after: Option<Instant>,
    protected_bytes: u64,
    max_queue_age: std::time::Duration,
}

impl Departures {
    fn accepted(&mut self, now: Instant, queued_at: Instant, bytes: usize) {
        self.protected_bytes = self
            .protected_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        self.max_queue_age = self
            .max_queue_age
            .max(now.saturating_duration_since(queued_at));
    }

    fn report(&mut self, now: Instant, budget: impl FnOnce() -> Option<wan::SendBudget>) {
        if self.protected_bytes == 0 || self.report_after.is_some_and(|after| now < after) {
            return;
        }
        self.report_after = now.checked_add(std::time::Duration::from_millis(250));
        self.flush();
        if let Some(budget) = budget() {
            metrics::histogram!("fabric_wan_egress_feedback_payload_budget_bits_per_second")
                .record(budget.payload_bps as f64);
        }
    }

    fn flush(&mut self) {
        if self.protected_bytes != 0 {
            metrics::counter!("fabric_wan_egress_protected_media_bytes")
                .increment(std::mem::take(&mut self.protected_bytes));
            metrics::histogram!("fabric_wan_egress_peak_departure_queue_age_seconds")
                .record(std::mem::take(&mut self.max_queue_age).as_secs_f64());
        }
    }
}

impl Drop for Departures {
    fn drop(&mut self) {
        self.flush();
    }
}

struct Active {
    route: Staged<Window>,
    protection: Option<Envelope>,
    deadline: Option<wan::LiveDeadline>,
    cutoff: BindingCutoff,
    parent: Option<ParentGate>,
}

impl Active {
    fn class(&self) -> Class {
        media_class(
            self.route.descriptor(),
            self.route.spec().purpose,
            self.protection.is_some(),
        )
    }

    fn expires(&self) -> std::time::Instant {
        let own = self
            .deadline
            .as_ref()
            .map_or_else(|| self.route.spec().expires, wan::LiveDeadline::expires);
        self.parent.as_ref().map_or(own, |parent| parent.limit(own))
    }

    fn is_live(&self) -> bool {
        self.cutoff.is_live() && self.parent.as_ref().is_none_or(ParentGate::is_live)
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.cutoff.fence();
    }
}

#[derive(Clone, Copy)]
struct Scope {
    binding: BindingId,
    window: Window,
    deadline: Instant,
}

pub(super) struct Media {
    scope: Scope,
    queued_at: Instant,
    position: u64,
    packet: OwnedPacket,
    omitted: Option<OmissionRun>,
}

struct Blocked {
    scope: Scope,
    queued_at: Instant,
    prepared: wan::Prepared,
    wait: Wait,
}

enum Wait {
    Writable,
    Congestion(Option<std::time::Instant>),
}

#[derive(Clone)]
pub(crate) struct ParentSend {
    pub cutoff: BindingCutoff,
    pub window: Window,
}

#[derive(Clone)]
pub(crate) enum SendParent {
    Transit {
        cutoff: BindingCutoff,
        window: Window,
    },
    Repair(ParentSend),
    Warm(super::warm::Fence),
}

#[derive(Clone)]
enum ParentGate {
    Local(BindingCutoff),
    Warm(super::warm::Fence),
}

impl ParentGate {
    fn is_live(&self) -> bool {
        match self {
            Self::Local(cutoff) => cutoff.is_live(),
            Self::Warm(fence) => fence.is_live(),
        }
    }

    fn limit(&self, expires: std::time::Instant) -> std::time::Instant {
        match self {
            Self::Local(_) => expires,
            Self::Warm(fence) => expires.min(fence.expires()),
        }
    }
}

impl SendParent {
    fn gate(&self) -> ParentGate {
        match self {
            Self::Transit { cutoff, .. } => ParentGate::Local(cutoff.clone()),
            Self::Repair(parent) => ParentGate::Local(parent.cutoff.clone()),
            Self::Warm(fence) => ParentGate::Warm(fence.clone()),
        }
    }
}

pub(crate) enum SendPolicy {
    Fixed,
    Renewable,
    Protected { wire_bps: NonZeroU64 },
}

pub(crate) struct Install {
    protected_wire_bps: Option<NonZeroU64>,
    cutoff: BindingCutoff,
    parent: Option<SendParent>,
    window: Window,
    acknowledged: Window,
    spec: WindowSpec,
    deadline: Option<wan::LiveDeadline>,
    report: oneshot::Sender<Result<(), Error>>,
}

#[derive(Clone)]
pub(crate) struct WanTx {
    cutoff: BindingCutoff,
    parent: Option<ParentGate>,
    window: Window,
    purpose: Purpose,
    expires: Instant,
    deadline: Option<wan::LiveDeadline>,
    class: Class,
    sender: pulsebeam_queue::Sender<Media>,
}

impl PartialEq for WanTx {
    fn eq(&self, other: &Self) -> bool {
        self.cutoff.id() == other.cutoff.id()
    }
}

impl Eq for WanTx {}

impl fmt::Debug for WanTx {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("WanTx")
            .field("cutoff", &self.cutoff)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
pub(crate) struct SendFixture {
    _links: pulsebeam_link::LinkControl<ShardFrame>,
    receiver: pulsebeam_queue::Receiver<Media>,
}

#[cfg(test)]
impl SendFixture {
    pub(crate) fn new(window: Window, expires: Instant) -> (Self, NetworkId, WanTx) {
        Self::with_capacity(window, expires, NonZeroUsize::MIN)
    }

    pub(crate) fn with_capacity(
        window: Window,
        expires: Instant,
        capacity: NonZeroUsize,
    ) -> (Self, NetworkId, WanTx) {
        let mut links = pulsebeam_link::LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
        let peer = links.reserve_network_id().unwrap();
        let (sender, receiver) = pulsebeam_queue::bounded(capacity);
        let tx = WanTx {
            cutoff: links.reserve_wan_send(peer).unwrap(),
            parent: None,
            window,
            purpose: Purpose::Live,
            expires,
            deadline: None,
            class: media_class(window, Purpose::Live, false),
            sender,
        };
        (
            Self {
                _links: links,
                receiver,
            },
            peer,
            tx,
        )
    }

    pub(crate) fn take(&mut self) -> Option<(u64, Option<OmissionRun>)> {
        let media = self.receiver.try_recv().ok()?;
        Some((media.position, media.omitted))
    }
}

impl WanTx {
    pub(crate) fn expires(&self) -> Instant {
        let own = self.deadline.as_ref().map_or(self.expires, |deadline| {
            Instant::from_std(deadline.expires())
        });
        self.parent.as_ref().map_or(own, |parent| {
            Instant::from_std(parent.limit(own.into_std()))
        })
    }

    pub(crate) fn renew(&self, now: Instant, expires: Instant) -> bool {
        self.is_live()
            && now < self.expires()
            && self
                .deadline
                .as_ref()
                .is_some_and(|deadline| deadline.renew(now.into_std(), expires.into_std()))
    }

    pub(crate) fn is_live(&self) -> bool {
        self.cutoff.is_live() && self.parent.as_ref().is_none_or(ParentGate::is_live)
    }

    pub(super) fn has_warm_parent(&self, exact: &super::warm::Fence) -> bool {
        self.is_protected_base()
            && self.is_live()
            && matches!(&self.parent, Some(ParentGate::Warm(fence)) if fence.matches(exact))
    }

    pub(crate) fn is_protected_base(&self) -> bool {
        self.class == Class::Protected
    }

    pub(crate) fn cutoff(&self) -> BindingCutoff {
        self.cutoff.clone()
    }

    pub(crate) fn covers(&self, position: u64) -> bool {
        match self.purpose {
            Purpose::Live => self.window.contains(position),
            Purpose::Repair { position: expected } => position == expected,
        }
    }

    pub(crate) fn accommodates_omission<P: AsRef<[u8]>, E: AsRef<[u8]>>(
        &self,
        position: u64,
        packet: &pulsebeam_core::fabric::FabricPacket<P, E>,
        run: OmissionRun,
    ) -> bool {
        self.purpose == Purpose::Live && self.window.accommodates_omission(position, packet, run)
    }

    pub(crate) fn try_send(
        &self,
        now: Instant,
        deadline: Instant,
        position: u64,
        packet: OwnedPacket,
    ) -> bool {
        self.try_send_projected(now, deadline, position, packet, None)
    }

    pub(crate) fn try_send_projected(
        &self,
        now: Instant,
        deadline: Instant,
        position: u64,
        packet: OwnedPacket,
        omitted: Option<OmissionRun>,
    ) -> bool {
        let deadline = deadline.min(self.expires());
        if deadline <= now
            || !self.is_live()
            || !self.covers(position)
            || (omitted.is_some() && self.purpose != Purpose::Live)
        {
            return false;
        }
        let queued = self
            .sender
            .try_send(Media {
                scope: Scope {
                    binding: self.cutoff.id(),
                    window: self.window,
                    deadline,
                },
                queued_at: now,
                position,
                packet,
                omitted,
            })
            .is_ok();
        if self.class == Class::Audio {
            opus_egress_event(if queued {
                "fabric_wan_opus_intake"
            } else {
                "fabric_wan_opus_intake_full"
            });
        }
        queued
    }
}

fn opus_egress_event(event: &'static str) {
    metrics::counter!(event).increment(1);
    #[cfg(feature = "sim")]
    crate::sim_metrics::record_routing_counter(event);
}

pub(crate) struct PendingTx {
    tx: Option<WanTx>,
    receipt: oneshot::Receiver<Result<(), Error>>,
}

impl PendingTx {
    pub(crate) fn poll(&mut self) -> Poll<Option<WanTx>> {
        if self.tx.is_none() {
            return Poll::Ready(None);
        }
        match self.receipt.try_recv() {
            Ok(Ok(())) => Poll::Ready(self.tx.take().filter(|tx| {
                let live = tx.is_live();
                if !live {
                    tx.cutoff.fence();
                }
                live
            })),
            Err(oneshot::error::TryRecvError::Empty) => Poll::Pending,
            _ => {
                if let Some(tx) = self.tx.take() {
                    tx.cutoff.fence();
                }
                Poll::Ready(None)
            }
        }
    }
}

impl Drop for PendingTx {
    fn drop(&mut self) {
        if let Some(tx) = &self.tx {
            tx.cutoff.fence();
        }
    }
}

impl Commands {
    /// The caller authenticates the peer ACK and scopes it to its own exact
    /// route authority. The socket owner supplies a second, path-local gate.
    pub(crate) fn install_send(
        &self,
        cutoff: BindingCutoff,
        window: Window,
        acknowledged: Window,
        spec: WindowSpec,
        policy: SendPolicy,
        parent: Option<SendParent>,
    ) -> Result<PendingTx, BindError> {
        let (report, receipt) = oneshot::channel();
        let deadline =
            (!matches!(policy, SendPolicy::Fixed)).then(|| wan::LiveDeadline::new(spec.expires));
        let protected_wire_bps = match policy {
            SendPolicy::Protected { wire_bps } => Some(wire_bps),
            _ => None,
        };
        let class = media_class(window, spec.purpose, protected_wire_bps.is_some());
        let tx = WanTx {
            cutoff: cutoff.clone(),
            parent: parent.as_ref().map(SendParent::gate),
            window,
            purpose: spec.purpose,
            expires: Instant::from_std(spec.expires),
            deadline: deadline.clone(),
            class,
            sender: self.media[class as usize].clone(),
        };
        let pending = PendingTx {
            tx: Some(tx),
            receipt,
        };
        self.try_send(Command::Send(Box::new(Install {
            protected_wire_bps,
            cutoff,
            parent,
            window,
            acknowledged,
            spec,
            deadline,
            report,
        })))?;
        Ok(pending)
    }
}

impl Egress {
    fn admits_protected(&self, now: Instant, wire_bps: NonZeroU64) -> bool {
        let Some(limit) = self.protected_egress_bps else {
            return false;
        };
        // Active windows are already bounded by the association. Charge the
        // same set that can actually depart, including both warm legs when
        // they carry distinct streams through this shared hop.
        self.routes
            .values()
            .filter(|active| active.is_live() && now.into_std() < active.expires())
            .filter_map(|active| active.protection.as_ref())
            .try_fold(limit.get(), |left, envelope| {
                left.checked_sub(envelope.rate())
            })
            .is_some_and(|available| available >= wire_bps.get())
    }

    fn priority_allows(&self, class: Class) -> bool {
        Class::PRIORITY
            .into_iter()
            .all(|priority| !priority.precedes(class) || self.blocked[priority as usize].is_none())
    }

    pub(super) async fn readable(&mut self) {
        if Class::ALL.into_iter().any(|class| {
            self.priority_allows(class)
                && self.blocked[class as usize].is_none()
                && self.queued.has_packets(class)
        }) {
            return;
        }
        async fn ready(inbox: &mut pulsebeam_queue::Receiver<Media>) {
            if !inbox.readable().await {
                std::future::pending::<()>().await;
            }
        }
        if let Some(MediaInbox {
            lanes: [audio, protected, live, repair],
            ..
        }) = &mut self.inbox
        {
            pulsebeam_runtime::select! {
                _ = ready(audio) => {},
                _ = ready(protected) => {},
                _ = ready(live) => {},
                _ = ready(repair) => {},
            }
        } else {
            std::future::pending::<()>().await;
        }
    }

    fn publish_budget(&mut self, now: Instant, snapshot: impl FnOnce() -> Option<wan::SendBudget>) {
        let Some(inbox) = &self.inbox else { return };
        if self.budget_at.is_some_and(|after| now < after) {
            return;
        }
        self.budget_at = now.checked_add(std::time::Duration::from_millis(100));
        let budget = snapshot().and_then(|snapshot| {
            let flows = self
                .routes
                .values()
                .filter(|active| {
                    active.is_live()
                        && now.into_std() < active.expires()
                        && active.route.spec().purpose == Purpose::Live
                        && active.route.descriptor().profile()
                            == pulsebeam_core::fabric::Profile::Vp9
                })
                .count();
            let share = (snapshot.payload_bps / 100)
                .saturating_mul(85)
                .checked_div(u64::try_from(flows).ok()?)?;
            Some(VideoBudget {
                share_bps: u32::try_from(share).unwrap_or(u32::MAX),
                expires: Instant::from_std(snapshot.valid_until),
            })
        });
        inbox.budget.send_replace(budget);
    }

    fn stage_media(&mut self, now: Instant) {
        for class in Class::ALL {
            for _ in 0..BUDGET {
                let Some(media) = self
                    .inbox
                    .as_mut()
                    .and_then(|inbox| inbox.lanes[class as usize].try_recv().ok())
                else {
                    break;
                };
                let Some(route) = self.route(now, media.scope) else {
                    if class == Class::Audio {
                        opus_egress_event("fabric_wan_opus_intake_stale");
                    }
                    continue;
                };
                if route.class() != class {
                    continue;
                }
                let window = media.scope.window;
                let Ok(cost) = Window::protected_len(&media.packet, media.omitted) else {
                    continue;
                };
                if !self.queued.push(class, window.ssrc(), cost, media) {
                    if class == Class::Audio {
                        opus_egress_event("fabric_wan_opus_schedule_full");
                    }
                }
            }
        }
    }

    pub(super) fn socket_write_blocked(&self) -> bool {
        Class::ALL.into_iter().any(|class| {
            self.priority_allows(class)
                && self.blocked[class as usize]
                    .as_ref()
                    .is_some_and(|blocked| matches!(blocked.wait, Wait::Writable))
        })
    }

    pub(super) fn next_deadline(&self) -> Option<std::time::Instant> {
        self.routes
            .values()
            .map(Active::expires)
            .chain(Class::ALL.into_iter().flat_map(|class| {
                self.blocked[class as usize]
                    .iter()
                    .flat_map(move |blocked| {
                        let retry = match blocked.wait {
                            Wait::Congestion(deadline) if self.priority_allows(class) => deadline,
                            _ => None,
                        };
                        [Some(blocked.scope.deadline.into_std()), retry]
                            .into_iter()
                            .flatten()
                    })
            }))
            .min()
    }

    fn route(&self, now: Instant, scope: Scope) -> Option<&Active> {
        let active = self.routes.get(&scope.window.ssrc())?;
        (now < scope.deadline
            && active.is_live()
            && now.into_std() < active.expires()
            && active.cutoff.id() == scope.binding
            && active.route.descriptor() == scope.window)
            .then_some(active)
    }

    // Only used after the exact route and scope have been checked by drain_media.
    fn protection(&mut self, scope: Scope) -> Option<&mut Envelope> {
        self.routes
            .get_mut(&scope.window.ssrc())?
            .protection
            .as_mut()
    }

    pub(super) fn fence_stale(
        &mut self,
        driver: &mut wan::Driver<FabricWanCodec, Window>,
        now: Instant,
        changed: bool,
    ) {
        self.routes.retain(|_, active| {
            if changed || now.into_std() >= active.expires() || !active.is_live() {
                active.cutoff.fence();
                driver.retire(active.route);
                false
            } else {
                true
            }
        });
        if changed {
            self.queued = Schedule::default();
            self.budget_at = None;
            if let Some(inbox) = &self.inbox {
                inbox.budget.send_replace(None);
            }
        }
        for class in Class::ALL {
            let index = class as usize;
            if self.blocked[index]
                .as_ref()
                .is_some_and(|blocked| self.route(now, blocked.scope).is_none())
            {
                self.blocked[index] = None;
            }
        }
    }
}

impl WanHost {
    pub(super) fn install_send(&mut self, now: Instant, install: Install) {
        let Install {
            protected_wire_bps,
            cutoff,
            parent,
            window,
            acknowledged,
            spec,
            deadline,
            report,
        } = install;
        if self
            .egress
            .routes
            .values()
            .any(|route| route.cutoff.id() == cutoff.id())
        {
            let _ = report.send(Err(Error::Binding));
            return;
        }
        let allowed_parent = match (spec.purpose, &parent) {
            (Purpose::Live, None) => true,
            (
                Purpose::Live,
                Some(SendParent::Transit {
                    cutoff,
                    window: input,
                }),
            ) => {
                cutoff.is_live()
                    && Window::new(
                        input.profile(),
                        input.start(),
                        input.forms(),
                        window.ssrc(),
                        input.emission(),
                    )
                    .ok()
                        == Some(window)
            }
            (Purpose::Live, Some(SendParent::Warm(fence))) => {
                protected_wire_bps.is_some()
                    && fence.is_live()
                    && now.into_std() < fence.expires()
                    && spec.expires <= fence.expires()
                    && spec.authority == fence.authority
                    && Window::new(
                        fence.window.profile(),
                        fence.window.start(),
                        fence.window.forms(),
                        window.ssrc(),
                        fence.window.emission(),
                    )
                    .ok()
                        == Some(window)
            }
            (Purpose::Repair { position }, Some(SendParent::Repair(parent))) => {
                let scope = Scope {
                    binding: parent.cutoff.id(),
                    window: parent.window,
                    deadline: Instant::from_std(spec.expires),
                };
                self.egress.route(now, scope).is_some_and(|route| {
                    let live = route.route.spec();
                    live.purpose == Purpose::Live
                        && live.authority == spec.authority
                        && spec.expires <= route.expires()
                        && parent.window.repair_window(position, window.ssrc()).ok() == Some(window)
                })
            }
            _ => false,
        };
        if window != acknowledged
            || !allowed_parent
            || !cutoff.is_live()
            || report.is_closed()
            || self.egress.routes.contains_key(&window.ssrc())
        {
            cutoff.fence();
            let _ = report.send(Err(Error::Binding));
            return;
        }
        if let Some(wire_bps) = protected_wire_bps {
            if spec.purpose != Purpose::Live || !window.profile().is_video() {
                cutoff.fence();
                let _ = report.send(Err(Error::Binding));
                return;
            }
            if !self.egress.admits_protected(now, wire_bps) {
                cutoff.fence();
                let _ = report.send(Err(Error::Capacity));
                return;
            }
        }
        let route = match self.stage_send_with_deadline(now, window, spec, deadline.clone()) {
            Ok(route) => route,
            Err(error) => {
                cutoff.fence();
                let _ = report.send(Err(error));
                return;
            }
        };
        let activated = self.activate_send(now, route, acknowledged, spec);
        let live = activated.is_ok();
        if report.send(activated).is_ok() && live {
            self.egress.routes.insert(
                window.ssrc(),
                Active {
                    route,
                    protection: protected_wire_bps.map(|rate| Envelope::new(rate, now)),
                    deadline,
                    cutoff,
                    parent: parent.as_ref().map(SendParent::gate),
                },
            );
        } else {
            cutoff.fence();
            self.driver.retire(route);
        }
    }

    pub(super) fn drain_media(&mut self, now: Instant) {
        self.egress
            .publish_budget(now, || self.driver.send_budget(now.into_std()));
        self.egress
            .departures
            .report(now, || self.driver.send_budget(now.into_std()));
        self.egress.stage_media(now);
        let priority_live =
            self.egress
                .routes
                .values()
                .fold([false; Class::ALL.len()], |mut live, active| {
                    let class = active.class();
                    if Class::PRIORITY.contains(&class)
                        && active.is_live()
                        && now.into_std() < active.expires()
                    {
                        live[class as usize] = true;
                    }
                    live
                });
        let mut deferred = [false; Class::ALL.len()];
        for _ in 0..BUDGET {
            // Waiting priority traffic gets the next pacer/window credit
            // instead of letting an ordinary send refill the window.
            let allowed = Class::ALL.map(|class| self.egress.priority_allows(class));
            let Some(class) = self.egress.queued.pick(|class, queued| {
                let i = class as usize;
                allowed[i] && !deferred[i] && self.egress.blocked[i].as_ref().map_or(queued, |blocked| {
                    !matches!(blocked.wait, Wait::Congestion(Some(deadline)) if now.into_std() < deadline)
                        || now >= blocked.scope.deadline
                })
            }) else { break };
            let index = class as usize;
            let Blocked {
                scope,
                queued_at,
                prepared,
                ..
            } = if let Some(blocked) = self.egress.blocked[index].take() {
                blocked
            } else {
                let Some(media) = self.egress.queued.pop(class) else {
                    break;
                };
                let Some(route) = self.egress.route(now, media.scope) else {
                    continue;
                };
                let Ok(prepared) = self.prepare_projected(
                    now,
                    route.route,
                    media.position,
                    &media.packet,
                    media.omitted,
                ) else {
                    continue;
                };
                Blocked {
                    scope: media.scope,
                    queued_at: media.queued_at,
                    prepared,
                    wait: Wait::Writable,
                }
            };
            if self.egress.route(now, scope).is_none() {
                continue;
            }
            let wire_len = prepared.wire_len();
            if class == Class::Protected
                && !self
                    .egress
                    .protection(scope)
                    .is_some_and(|rate| rate.allows(now, wire_len))
            {
                metrics::counter!("fabric_wan_protected_envelope_dropped").increment(1);
                continue;
            }
            let reserve = if Class::PRIORITY
                .into_iter()
                .any(|priority| priority_live[priority as usize] && priority.precedes(class))
            {
                wan::MAX_DATAGRAM_BYTES
            } else {
                0
            };
            let (prepared, wait) = match self.submit_with_reserve(now, prepared, reserve) {
                Submission::Retry(prepared) => (prepared, Wait::Writable),
                Submission::Deferred { prepared, deadline } => {
                    if class == Class::Audio {
                        opus_egress_event("fabric_wan_opus_deferred");
                    }
                    (prepared, Wait::Congestion(deadline))
                }
                Submission::Accepted { bytes } => {
                    if class == Class::Protected
                        && let Some(rate) = self.egress.protection(scope)
                    {
                        rate.sent(bytes);
                    }
                    if class == Class::Audio {
                        opus_egress_event("fabric_wan_opus_sent");
                    }
                    self.egress.departures.accepted(now, queued_at, bytes);
                    continue;
                }
                Submission::Dropped(outcome) => {
                    if class == Class::Protected
                        && outcome == wan::Dropped::Uncertain
                        && let Some(rate) = self.egress.protection(scope)
                    {
                        rate.sent(wire_len);
                    }
                    if class == Class::Audio {
                        opus_egress_event("fabric_wan_opus_transport_dropped");
                    }
                    continue;
                }
            };
            self.egress.blocked[index] = Some(Blocked {
                scope,
                queued_at,
                prepared,
                wait,
            });
            deferred[index] = true;
        }
    }
}
