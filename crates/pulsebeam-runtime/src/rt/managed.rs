use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time::{Instant, Sleep};

/// Budgeted application logic. A poll may complete a tick but must not block.
/// Runtime-affine waits live in `ManagedIo`, not in suspended application state.
pub trait Service: Unpin + 'static {
    fn poll_service(&mut self, io: &mut ManagedIo, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
}

/// Resources belonging to one logical service across worker placements.
pub struct ManagedIo {
    socket: Option<pulsebeam_udp::UdpSocket>,
    #[cfg(all(target_os = "linux", not(feature = "sim")))]
    detached: Option<pulsebeam_udp::DetachedUdpSocket>,
    timer: Option<Pin<Box<Sleep>>>,
    deadline: Option<Instant>,
    runnable_since: Option<Instant>,
    scheduling_wait: std::time::Duration,
    #[cfg(all(test, target_os = "linux", not(feature = "sim")))]
    fail_detach: bool,
    #[cfg(all(test, target_os = "linux", not(feature = "sim")))]
    fail_activations: usize,
}

impl ManagedIo {
    pub fn new(socket: pulsebeam_udp::UdpSocket) -> Self {
        Self {
            socket: Some(socket),
            #[cfg(all(target_os = "linux", not(feature = "sim")))]
            detached: None,
            timer: None,
            deadline: None,
            runnable_since: None,
            scheduling_wait: std::time::Duration::ZERO,
            #[cfg(all(test, target_os = "linux", not(feature = "sim")))]
            fail_detach: false,
            #[cfg(all(test, target_os = "linux", not(feature = "sim")))]
            fail_activations: 0,
        }
    }

    pub fn socket(&mut self) -> &mut pulsebeam_udp::UdpSocket {
        // Only an attached envelope may poll application logic.
        #[allow(
            clippy::expect_used,
            reason = "only attached envelopes may poll application logic"
        )]
        self.socket.as_mut().expect("polling detached service")
    }

    /// End a budgeted tick and record only runnable-to-poll delay, excluding idle waits.
    pub fn yield_after_tick(&mut self, cx: &mut Context<'_>) {
        self.runnable_since = Some(Instant::now());
        cx.waker().wake_by_ref();
    }

    pub async fn readable(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        if self.deadline != deadline {
            self.timer = None;
            self.deadline = deadline;
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return Ok(());
            }
            self.timer
                .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
        }
        std::future::poll_fn(|cx| {
            let Some(socket) = self.socket.as_ref() else {
                return Poll::Ready(Err(io::Error::other("waiting on detached service")));
            };
            if let Poll::Ready(result) = socket.poll_readable(cx) {
                return Poll::Ready(result);
            }
            match self.timer.as_mut() {
                Some(timer) => timer.as_mut().poll(cx).map(|()| Ok(())),
                None => Poll::Pending,
            }
        })
        .await
    }

    pub fn poll_deadline(&mut self, cx: &mut Context<'_>, deadline: Option<Instant>) -> Poll<()> {
        if self.deadline != deadline {
            self.timer = None;
            self.deadline = deadline;
        }
        let Some(deadline) = deadline else {
            return Poll::Pending;
        };
        if Instant::now() >= deadline {
            return Poll::Ready(());
        }
        self.timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)))
            .as_mut()
            .poll(cx)
    }

    #[cfg(not(feature = "sim"))]
    fn detach(&mut self) -> io::Result<()> {
        #[cfg(all(test, target_os = "linux", not(feature = "sim")))]
        if std::mem::take(&mut self.fail_detach) {
            return Err(io::Error::other("injected detach failure"));
        }
        #[cfg(all(target_os = "linux", not(feature = "sim")))]
        {
            self.timer = None;
            if let Some(socket) = self.socket.take() {
                self.detached = Some(socket.detach());
            }
            Ok(())
        }
        #[cfg(not(all(target_os = "linux", not(feature = "sim"))))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cross-reactor transfer requires native Linux sockets",
        ))
    }

    #[cfg(not(feature = "sim"))]
    fn attach(&mut self) -> io::Result<()> {
        #[cfg(all(test, target_os = "linux", not(feature = "sim")))]
        if self.fail_activations > 0 {
            self.fail_activations = self.fail_activations.saturating_sub(1);
            return Err(io::Error::other("injected registration failure"));
        }
        #[cfg(all(target_os = "linux", not(feature = "sim")))]
        {
            let Some(socket) = self.detached.take() else {
                return Ok(());
            };
            match socket.activate() {
                Ok(socket) => {
                    self.socket = Some(socket);
                    Ok(())
                }
                Err((socket, error)) => {
                    self.detached = Some(*socket);
                    Err(error)
                }
            }
        }
        #[cfg(not(all(target_os = "linux", not(feature = "sim"))))]
        Ok(())
    }
}

/// Created once and pinned once. Handoffs move the owning box, never its contents.
pub struct ManagedService<S> {
    service: S,
    io: ManagedIo,
}

impl<S: Service> ManagedService<S> {
    #[cfg(all(test, target_os = "linux", not(feature = "sim")))]
    pub(super) fn inject_failures(mut self, detach: bool, activations: usize) -> Self {
        self.io.fail_detach = detach;
        self.io.fail_activations = activations;
        self
    }

    pub fn new(service: S, socket: pulsebeam_udp::UdpSocket) -> Self {
        Self {
            service,
            io: ManagedIo::new(socket),
        }
    }
}

impl<S: Service> Future for ManagedService<S> {
    type Output = io::Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(since) = this.io.runnable_since.take() {
            this.io.scheduling_wait = this.io.scheduling_wait.saturating_add(since.elapsed());
        }
        this.service.poll_service(&mut this.io, cx)
    }
}

pub(super) trait Transferable: Future<Output = io::Result<()>> + Send {
    fn detach(self: Pin<&mut Self>) -> io::Result<()>;
    fn attach(self: Pin<&mut Self>) -> io::Result<()>;
    fn scheduling_wait(&self) -> std::time::Duration;
}

#[cfg(not(feature = "sim"))]
impl<S: Service + Send> Transferable for ManagedService<S> {
    fn detach(self: Pin<&mut Self>) -> io::Result<()> {
        self.get_mut().io.detach()
    }
    fn attach(self: Pin<&mut Self>) -> io::Result<()> {
        self.get_mut().io.attach()
    }
    fn scheduling_wait(&self) -> std::time::Duration {
        self.io.scheduling_wait
    }
}

pub(super) type ServiceFuture = Pin<Box<dyn Transferable>>;

#[cfg(all(test, target_os = "linux", not(feature = "sim")))]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};

    #[allow(clippy::unwrap_used, clippy::panic)]
    #[tokio::test]
    async fn udp_wakes_a_parked_service_after_wait_future_drop_and_reattachment() {
        struct Signal(tokio::sync::Notify);
        impl std::task::Wake for Signal {
            fn wake(self: Arc<Self>) {
                self.0.notify_one();
            }
        }
        let socket = pulsebeam_udp::bind(pulsebeam_udp::BindOptions::new(
            "127.0.0.1:0".parse().unwrap(),
        ))
        .await
        .unwrap();
        let destination = socket.local_addr();
        let mut io = ManagedIo::new(socket.activate().unwrap());
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let signal = Arc::new(Signal(tokio::sync::Notify::new()));
        let waker = std::task::Waker::from(signal.clone());
        let mut cx = Context::from_waker(&waker);
        for reattach in [false, true] {
            if reattach {
                io.detach().unwrap();
                io.attach().unwrap();
            }
            match io.socket().try_recv_batch() {
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
                Ok(_) => panic!("unexpected queued datagram"),
            }
            {
                let wait = io.readable(None);
                tokio::pin!(wait);
                assert!(wait.as_mut().poll(&mut cx).is_pending());
            }
            sender.send_to(b"wake", destination).await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), signal.0.notified())
                .await
                .unwrap();
            assert_eq!(io.socket().try_recv_batch().unwrap().iter().count(), 1);
        }
    }
}
