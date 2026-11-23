use std::{fmt, task::Poll};

use pulsebeam_link::{BindError, BindingCutoff, BindingId};
use pulsebeam_runtime::sync::oneshot;

use super::*;

const BUDGET: usize = 32;

#[derive(Default)]
pub(super) struct Egress {
    pub(super) inbox: Option<pulsebeam_queue::Receiver<Media>>,
    routes: BTreeMap<NonZeroU32, Active>,
    blocked: Option<Blocked>,
}

struct Active {
    route: Staged<Window>,
    cutoff: BindingCutoff,
    parent: Option<BindingCutoff>,
}

impl Active {
    fn is_live(&self) -> bool {
        self.cutoff.is_live() && self.parent.as_ref().is_none_or(BindingCutoff::is_live)
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
    position: u64,
    packet: OwnedPacket,
}

struct Blocked {
    scope: Scope,
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
}

impl SendParent {
    fn cutoff(&self) -> &BindingCutoff {
        match self {
            Self::Transit { cutoff, .. } => cutoff,
            Self::Repair(parent) => &parent.cutoff,
        }
    }
}

pub(crate) struct Install {
    cutoff: BindingCutoff,
    parent: Option<SendParent>,
    window: Window,
    acknowledged: Window,
    spec: WindowSpec,
    report: oneshot::Sender<Result<(), Error>>,
}

#[derive(Clone)]
pub(crate) struct WanTx {
    cutoff: BindingCutoff,
    parent: Option<BindingCutoff>,
    window: Window,
    purpose: Purpose,
    expires: Instant,
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
    _receiver: pulsebeam_queue::Receiver<Media>,
}

#[cfg(test)]
impl SendFixture {
    pub(crate) fn new(window: Window, expires: Instant) -> (Self, NetworkId, WanTx) {
        let mut links = pulsebeam_link::LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
        let peer = links.reserve_network_id().unwrap();
        let (sender, receiver) = pulsebeam_queue::bounded(NonZeroUsize::MIN);
        let tx = WanTx {
            cutoff: links.reserve_wan_send(peer).unwrap(),
            parent: None,
            window,
            purpose: Purpose::Live,
            expires,
            sender,
        };
        (
            Self {
                _links: links,
                _receiver: receiver,
            },
            peer,
            tx,
        )
    }
}

impl WanTx {
    pub(crate) fn is_live(&self) -> bool {
        self.cutoff.is_live() && self.parent.as_ref().is_none_or(BindingCutoff::is_live)
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

    pub(crate) fn try_send(
        &self,
        now: Instant,
        deadline: Instant,
        position: u64,
        packet: OwnedPacket,
    ) -> bool {
        let deadline = deadline.min(self.expires);
        if deadline <= now || !self.is_live() || !self.covers(position) {
            return false;
        }
        self.sender
            .try_send(Media {
                scope: Scope {
                    binding: self.cutoff.id(),
                    window: self.window,
                    deadline,
                },
                position,
                packet,
            })
            .is_ok()
    }
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
        parent: Option<SendParent>,
    ) -> Result<PendingTx, BindError> {
        let (report, receipt) = oneshot::channel();
        let tx = WanTx {
            cutoff: cutoff.clone(),
            parent: parent.as_ref().map(|parent| parent.cutoff().clone()),
            window,
            purpose: spec.purpose,
            expires: Instant::from_std(spec.expires),
            sender: self.media.clone(),
        };
        let pending = PendingTx {
            tx: Some(tx),
            receipt,
        };
        self.try_send(Command::Send(Box::new(Install {
            cutoff,
            parent,
            window,
            acknowledged,
            spec,
            report,
        })))?;
        Ok(pending)
    }
}

impl Egress {
    pub(super) fn has_blocked(&self) -> bool {
        self.blocked.is_some()
    }

    pub(super) fn socket_write_blocked(&self) -> bool {
        self.blocked
            .as_ref()
            .is_some_and(|blocked| matches!(blocked.wait, Wait::Writable))
    }

    pub(super) fn next_deadline(&self) -> Option<std::time::Instant> {
        self.routes
            .values()
            .map(|active| active.route.spec().expires)
            .chain(
                self.blocked
                    .as_ref()
                    .map(|blocked| blocked.scope.deadline.into_std()),
            )
            .chain(
                self.blocked
                    .as_ref()
                    .and_then(|blocked| match blocked.wait {
                        Wait::Writable => None,
                        Wait::Congestion(deadline) => deadline,
                    }),
            )
            .min()
    }

    fn route(&self, now: Instant, scope: Scope) -> Option<Staged<Window>> {
        let active = self.routes.get(&scope.window.ssrc())?;
        (now < scope.deadline
            && active.is_live()
            && now.into_std() < active.route.spec().expires
            && active.cutoff.id() == scope.binding
            && active.route.descriptor() == scope.window)
            .then_some(active.route)
    }

    pub(super) fn fence_stale(
        &mut self,
        driver: &mut wan::Driver<FabricWanCodec, Window>,
        now: Instant,
        changed: bool,
    ) {
        self.routes.retain(|_, active| {
            if changed || now.into_std() >= active.route.spec().expires || !active.is_live() {
                active.cutoff.fence();
                driver.retire(active.route);
                false
            } else {
                true
            }
        });
        if self
            .blocked
            .as_ref()
            .is_some_and(|blocked| self.route(now, blocked.scope).is_none())
        {
            self.blocked = None;
        }
    }
}

impl WanHost {
    pub(super) fn install_send(&mut self, now: Instant, install: Install) {
        let Install {
            cutoff,
            parent,
            window,
            acknowledged,
            spec,
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
            (Purpose::Repair { position }, Some(SendParent::Repair(parent))) => {
                let scope = Scope {
                    binding: parent.cutoff.id(),
                    window: parent.window,
                    deadline: Instant::from_std(spec.expires),
                };
                self.egress.route(now, scope).is_some_and(|route| {
                    let live = route.spec();
                    live.purpose == Purpose::Live
                        && live.authority == spec.authority
                        && spec.expires <= live.expires
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
        let route = match self.stage_send(now, window, spec) {
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
                    cutoff,
                    parent: parent.map(|parent| parent.cutoff().clone()),
                },
            );
        } else {
            cutoff.fence();
            self.driver.retire(route);
        }
    }

    pub(super) fn drain_media(&mut self, now: Instant) {
        for _ in 0..BUDGET {
            let Blocked {
                scope, prepared, ..
            } = if let Some(blocked) = self.egress.blocked.take() {
                blocked
            } else {
                let Some(media) = self
                    .egress
                    .inbox
                    .as_mut()
                    .and_then(|inbox| inbox.try_recv().ok())
                else {
                    break;
                };
                let Some(route) = self.egress.route(now, media.scope) else {
                    continue;
                };
                let Ok(prepared) = self.prepare(now, route, media.position, &media.packet) else {
                    continue;
                };
                Blocked {
                    scope: media.scope,
                    prepared,
                    wait: Wait::Writable,
                }
            };
            if self.egress.route(now, scope).is_none() {
                continue;
            }
            let (prepared, wait) = match self.submit(now, prepared) {
                Submission::Retry(prepared) => (prepared, Wait::Writable),
                Submission::Deferred { prepared, deadline } => {
                    (prepared, Wait::Congestion(deadline))
                }
                Submission::Accepted { .. } | Submission::Dropped(_) => continue,
            };
            self.egress.blocked = Some(Blocked {
                scope,
                prepared,
                wait,
            });
            break;
        }
    }
}
