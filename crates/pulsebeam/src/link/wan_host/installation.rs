use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use pulsebeam_runtime::sync::oneshot;

use super::*;

type Report = oneshot::Sender<Result<Window, Error>>;

pub(crate) struct Commands {
    peer: NetworkId,
    sender: pulsebeam_queue::Sender<Command>,
    pub(super) media: pulsebeam_queue::Sender<super::egress::Media>,
}

impl Commands {
    pub(super) fn new(
        peer: NetworkId,
        sender: pulsebeam_queue::Sender<Command>,
        media: pulsebeam_queue::Sender<super::egress::Media>,
    ) -> Self {
        Self {
            peer,
            sender,
            media,
        }
    }

    pub(crate) fn peer(&self) -> NetworkId {
        self.peer
    }

    pub(crate) fn try_send(
        &self,
        command: impl Into<Command>,
    ) -> Result<(), pulsebeam_link::BindError> {
        self.sender
            .try_send(command.into())
            .map_err(|error| match error {
                pulsebeam_queue::TrySendError::Full(_) => pulsebeam_link::BindError::Capacity,
                pulsebeam_queue::TrySendError::Closed(_) => pulsebeam_link::BindError::Closed,
            })
    }
}

pub(crate) struct ChildInstall<T> {
    pub child: T,
    pub tx: LinkTx<ShardFrame>,
    pub receipt: oneshot::Receiver<Option<T>>,
}

pub(crate) enum Command {
    Receive(ReceiveCommand),
    Send(Box<super::egress::Install>),
}

impl From<ReceiveCommand> for Command {
    fn from(command: ReceiveCommand) -> Self {
        Self::Receive(command)
    }
}

pub(crate) enum ReceiveCommand {
    Live {
        expires: Instant,
        timing: Option<crate::route::fabric::WindowTiming>,
        children: Vec<ChildInstall<LiveNetworkChild>>,
        report: Report,
    },
    Repair {
        parent: LiveNetworkChild,
        install: Box<ChildInstall<RepairChild>>,
        report: Report,
    },
    MediaRepair {
        install: Box<ChildInstall<MediaRepairChild>>,
        report: Report,
    },
}

impl ReceiveCommand {
    fn cutoffs(&self, host: &WanHost) -> Vec<pulsebeam_link::BindingCutoff> {
        let cutoff = |tx: &LinkTx<ShardFrame>| {
            if host.receive_tx_in_use(tx) {
                None
            } else {
                tx.local_cutoff()
            }
        };
        match self {
            Self::Live { children, .. } => children
                .iter()
                .filter_map(|child| cutoff(&child.tx))
                .collect(),
            Self::Repair { install, .. } => cutoff(&install.tx).into_iter().collect(),
            Self::MediaRepair { install, .. } => cutoff(&install.tx).into_iter().collect(),
        }
    }

    fn report(self) -> Report {
        match self {
            Self::Live { report, .. }
            | Self::Repair { report, .. }
            | Self::MediaRepair { report, .. } => report,
        }
    }

    fn window(&self) -> Option<Window> {
        match self {
            Self::Live { children, .. } => children.first()?.child.window,
            Self::Repair { install, .. } => install.child.window,
            Self::MediaRepair { install, .. } => Some(install.child.window),
        }
    }
}

type Receipts<T> = Vec<(T, oneshot::Receiver<Option<T>>)>;

pub(super) struct Pending {
    path: Lease,
    report: Report,
    route: Route,
    outcome: Poll<bool>,
}

enum Route {
    Live(Box<LiveReceive>, Receipts<LiveNetworkChild>),
    Repair(Box<PendingRepair>),
}

struct PendingRepair {
    route: RepairReceive,
    receipt: RepairReceipt,
}

enum RepairReceipt {
    Data(RepairChild, oneshot::Receiver<Option<RepairChild>>),
    Media(
        MediaRepairChild,
        oneshot::Receiver<Option<MediaRepairChild>>,
    ),
}

impl RepairReceipt {
    fn poll(&mut self, cx: Option<&mut Context<'_>>) -> Poll<bool> {
        match self {
            Self::Data(child, receipt) => poll_receipt(*child, receipt, cx),
            Self::Media(child, receipt) => poll_receipt(*child, receipt, cx),
        }
    }
}

impl Route {
    fn ticket(&self) -> Staged<Window> {
        match self {
            Self::Live(route, _) => route.window,
            Self::Repair(repair) => repair.route.window,
        }
    }

    fn fence(&self) {
        match self {
            Self::Live(route, _) => fence_receive(route),
            Self::Repair(repair) => fence_destination(&repair.route.tx),
        }
    }

    fn uses(&self, tx: &LinkTx<ShardFrame>) -> bool {
        match self {
            Self::Live(route, _) => route
                .destinations
                .iter()
                .any(|destination| destination.tx == *tx),
            Self::Repair(repair) => repair.route.tx == *tx,
        }
    }

    fn poll(&mut self, mut cx: Option<&mut Context<'_>>) -> Poll<bool> {
        match self {
            Self::Live(_, receipts) => {
                let mut accepted = true;
                receipts.retain_mut(|(expected, receipt)| {
                    match poll_receipt(*expected, receipt, cx.as_deref_mut()) {
                        Poll::Pending => true,
                        Poll::Ready(acknowledged) => {
                            accepted &= acknowledged;
                            false
                        }
                    }
                });
                if !accepted || receipts.is_empty() {
                    Poll::Ready(accepted)
                } else {
                    Poll::Pending
                }
            }
            Self::Repair(repair) => repair.receipt.poll(cx),
        }
    }
}

fn poll_receipt<T: Copy + Eq>(
    expected: T,
    receipt: &mut oneshot::Receiver<Option<T>>,
    cx: Option<&mut Context<'_>>,
) -> Poll<bool> {
    if let Some(cx) = cx {
        return Pin::new(receipt)
            .poll(cx)
            .map(|result| result.ok() == Some(Some(expected)));
    }
    match receipt.try_recv() {
        Ok(ack) => Poll::Ready(ack == Some(expected)),
        Err(oneshot::error::TryRecvError::Empty) => Poll::Pending,
        Err(oneshot::error::TryRecvError::Closed) => Poll::Ready(false),
    }
}

impl Pending {
    pub(super) fn expires(&self) -> std::time::Instant {
        self.route.ticket().spec().expires
    }

    fn poll(&mut self, cx: Option<&mut Context<'_>>) -> Poll<bool> {
        if self.outcome.is_pending() {
            self.outcome = self.route.poll(cx);
        }
        self.outcome
    }

    pub(super) fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.report.poll_closed(cx).is_ready() {
            return Poll::Ready(());
        }
        self.poll(Some(cx)).map(|_| ())
    }
}

impl WanHost {
    /// The controller sends actual shard-update receipt receivers here. No
    /// transport route becomes usable until all exact child installs commit.
    pub(crate) fn install(&mut self, now: Instant, command: ReceiveCommand) {
        self.fence_stale(now);
        let cutoffs = command.cutoffs(self);
        let reject = |report: Report, error| {
            for cutoff in &cutoffs {
                cutoff.fence();
            }
            let _ = report.send(Err(error));
        };
        let Some(path) = self.driver.lease(now.into_std()) else {
            reject(command.report(), Error::Binding);
            return;
        };
        let Some(window) = command.window() else {
            reject(command.report(), Error::Binding);
            return;
        };
        let ssrc = window.ssrc();
        if self.pending.contains_key(&ssrc)
            || self.receives.contains_key(&ssrc)
            || self.repairs.contains_key(&ssrc)
        {
            reject(command.report(), Error::Binding);
            return;
        }
        if self.pending.len() == self.pending_capacity {
            reject(command.report(), Error::Capacity);
            return;
        }
        let (result, report) = match command {
            ReceiveCommand::Live {
                expires,
                timing,
                children,
                report,
            } => {
                let (destinations, receipts) = children
                    .into_iter()
                    .map(|ChildInstall { child, tx, receipt }| {
                        (Destination { child, tx }, (child, receipt))
                    })
                    .unzip();
                let result = self
                    .stage_live_receive(now, expires, timing, destinations)
                    .map(|route| Route::Live(Box::new(route), receipts));
                (result, report)
            }
            ReceiveCommand::Repair {
                parent,
                install,
                report,
            } => {
                let ChildInstall { child, tx, receipt } = *install;
                let result = self
                    .stage_repair_receive(now, parent, child, tx)
                    .map(|route| {
                        Route::Repair(Box::new(PendingRepair {
                            route,
                            receipt: RepairReceipt::Data(child, receipt),
                        }))
                    });
                (result, report)
            }
            ReceiveCommand::MediaRepair { install, report } => {
                let ChildInstall { child, tx, receipt } = *install;
                let result = self
                    .stage_media_repair_receive(now, child, tx)
                    .map(|route| {
                        Route::Repair(Box::new(PendingRepair {
                            route,
                            receipt: RepairReceipt::Media(child, receipt),
                        }))
                    });
                (result, report)
            }
        };
        match result {
            Ok(route) => {
                self.pending.insert(
                    ssrc,
                    Pending {
                        path,
                        report,
                        route,
                        outcome: Poll::Pending,
                    },
                );
            }
            Err(error) => {
                reject(report, error);
            }
        }
    }

    pub(super) fn receive_tx_in_use(&self, tx: &LinkTx<ShardFrame>) -> bool {
        self.receives.values().any(|route| {
            route
                .destinations
                .iter()
                .any(|destination| destination.tx == *tx)
        }) || self.repairs.values().any(|route| route.tx == *tx)
            || self.pending.values().any(|pending| pending.route.uses(tx))
    }

    pub(super) fn advance_installations(&mut self, now: Instant) {
        let lease = self.driver.lease(now.into_std());
        let mut finished = Vec::new();
        for (&ssrc, pending) in &mut self.pending {
            let ready = if lease != Some(pending.path)
                || now.into_std() >= pending.expires()
                || pending.report.is_closed()
            {
                Poll::Ready(false)
            } else {
                pending.poll(None)
            };
            if let Poll::Ready(accepted) = ready {
                finished.push((ssrc, accepted));
            }
        }
        for (ssrc, acknowledged) in finished {
            let Some(pending) = self.pending.remove(&ssrc) else {
                continue;
            };
            self.finish_install(now, pending, acknowledged);
        }
    }

    fn finish_install(&mut self, now: Instant, pending: Pending, acknowledged: bool) {
        let Pending { route, report, .. } = pending;
        let ticket = route.ticket();
        let live = match &route {
            Route::Live(route, _) => route
                .destinations
                .iter()
                .all(|destination| destination.tx.is_live()),
            Route::Repair(repair) => {
                let route = &repair.route;
                route.tx.is_live() && Self::has_live_parent(&self.receives, route.parent, &route.tx)
            }
        };
        let result = if acknowledged && live {
            self.driver
                .activate(now.into_std(), ticket, ticket.descriptor(), ticket.spec())
                .map(|()| ticket.descriptor())
                .map_err(Error::from)
        } else {
            Err(Error::Binding)
        };
        let installed = result.is_ok();
        if report.send(result).is_ok() && installed {
            let ssrc = ticket.spec().ssrc;
            match route {
                Route::Live(route, _) => {
                    self.receives.insert(ssrc, *route);
                }
                Route::Repair(repair) => {
                    self.repairs.insert(ssrc, repair.route);
                }
            }
        } else {
            route.fence();
            self.driver.retire(ticket);
        }
    }
}
