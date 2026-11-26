use super::*;
use agent_core::ffi::{
    ErrorCode, ErrorInfo, PublicationAdmissionStatus, PublicationExecution,
    SubscriptionSourceStatus,
};

pub(crate) enum FrameState {
    Video(Arc<agent_core::observer::VideoSubscriptionStateObserver>),
    Audio(Arc<agent_core::observer::AudioSubscriptionStateObserver>),
}

enum FrameChanges {
    Video(agent_core::observer::VideoSubscriptionUpdates),
    Audio(agent_core::observer::AudioSubscriptionUpdates),
}

fn operation(code: ErrorCode, message: &str) -> Error {
    Error::Core(agent_core::AgentError::OperationFailed(ErrorInfo::new(
        code, message,
    )))
}

impl FrameState {
    fn closed(&self) -> bool {
        match self {
            Self::Video(observer) => observer.state().closed,
            Self::Audio(observer) => observer.state().closed,
        }
    }

    fn failure(&self) -> Option<ErrorInfo> {
        macro_rules! failure {
            ($observer:expr) => {{
                let state = $observer.state();
                if state.execution == PublicationExecution::Failed {
                    state.execution_error
                } else if state.admission == PublicationAdmissionStatus::Rejected
                    || state.source_status == SubscriptionSourceStatus::Rejected
                {
                    state.error
                } else {
                    None
                }
            }};
        }
        match self {
            Self::Video(observer) => failure!(observer),
            Self::Audio(observer) => failure!(observer),
        }
    }

    fn updates(&self) -> Result<FrameChanges, Error> {
        let limit = |_| {
            operation(
                ErrorCode::CapacityExceeded,
                "Frame observation capacity exceeded",
            )
        };
        match self {
            Self::Video(observer) => Ok(FrameChanges::Video(observer.updates().map_err(limit)?)),
            Self::Audio(observer) => Ok(FrameChanges::Audio(observer.updates().map_err(limit)?)),
        }
    }
}

impl FrameChanges {
    async fn next(&self) -> Result<(), Error> {
        let conflict = |_| operation(ErrorCode::Conflict, "A frame-state read is already pending");
        match self {
            Self::Video(updates) => {
                updates.next().map_err(conflict)?.await;
            }
            Self::Audio(updates) => {
                updates.next().map_err(conflict)?.await;
            }
        }
        Ok(())
    }

    fn close(&self) {
        match self {
            Self::Video(updates) => updates.close(),
            Self::Audio(updates) => updates.close(),
        }
    }
}

enum FrameOrigin {
    Exact {
        subscription: Subscription,
        state: FrameState,
        changes: FrameChanges,
    },
    Admitted {
        room: RoomRef,
        admission: Box<agent_core::AdmittedMedia>,
    },
}

impl FrameOrigin {
    fn closed(&self) -> bool {
        match self {
            Self::Exact {
                subscription,
                state,
                ..
            } => subscription.is_closed() || state.closed(),
            Self::Admitted { .. } => false,
        }
    }

    async fn observe(&self) -> Result<RemoteMedia, Error> {
        match self {
            Self::Exact { subscription, .. } => subscription.observe(ReceiveMode::Decoded).await,
            Self::Admitted { room, admission } => {
                room.observe_admitted_media((**admission).clone()).await
            }
        }
    }

    async fn changed(&self) -> Result<(), Error> {
        match self {
            Self::Exact {
                subscription,
                changes,
                ..
            } => tokio::select! {
                _ = subscription.inner.closed.wait() => Ok(()),
                result = changes.next() => result,
            },
            Self::Admitted { .. } => core::future::pending().await,
        }
    }
}

pub(super) struct FrameReaderLease {
    origin: FrameOrigin,
    remote: std::sync::OnceLock<RemoteMedia>,
    failure: std::sync::OnceLock<ErrorInfo>,
    closed: Signal,
    reading: Mutex<()>,
}

impl FrameReaderLease {
    fn capture_failure(&self) {
        if let FrameOrigin::Exact { state, .. } = &self.origin
            && let Some(error) = state.failure()
        {
            let _ = self.failure.set(error);
        }
    }

    fn result(&self, frame: Option<RemoteFrame>) -> Result<Option<RemoteFrame>, Error> {
        self.capture_failure();
        if let Some(error) = self.failure.get() {
            return Err(Error::Core(agent_core::AgentError::OperationFailed(
                error.clone(),
            )));
        }
        if self.is_closed() {
            return Ok(None);
        }
        Ok(frame)
    }

    fn is_closed(&self) -> bool {
        self.closed.is_closed() || self.origin.closed()
    }

    pub(super) fn close(&self) {
        self.capture_failure();
        self.closed.close();
        if let FrameOrigin::Exact { changes, .. } = &self.origin {
            changes.close();
        }
        if let Some(remote) = self.remote.get() {
            remote.lease.close();
            remote.lease.leases_changed.notify_one();
        }
    }
}

impl Drop for FrameReaderLease {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) struct FrameReader {
    inner: Arc<FrameReaderLease>,
}

impl FrameReader {
    pub(crate) fn new(subscription: Subscription, state: FrameState) -> Result<Self, Error> {
        subscription.refresh_state();
        if subscription.is_closed() || state.closed() {
            return Err(Error::Closed);
        }
        let mut slot = subscription
            .inner
            .frame_reader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .upgrade()
            .is_some_and(|reader| !reader.closed.is_closed())
        {
            return Err(operation(
                ErrorCode::Conflict,
                "A frame reader already owns this subscription",
            ));
        }
        let changes = state.updates()?;
        let reader = Self::from_origin(FrameOrigin::Exact {
            subscription: subscription.clone(),
            state,
            changes,
        });
        *slot = Arc::downgrade(&reader.inner);
        Ok(reader)
    }

    pub(crate) fn admitted(
        room: RoomRef,
        admission: agent_core::AdmittedMedia,
    ) -> Result<Self, Error> {
        room.check_admitted_media(&admission)?;
        Ok(Self::from_origin(FrameOrigin::Admitted {
            room,
            admission: Box::new(admission),
        }))
    }

    fn from_origin(origin: FrameOrigin) -> Self {
        Self {
            inner: Arc::new(FrameReaderLease {
                origin,
                remote: std::sync::OnceLock::new(),
                failure: std::sync::OnceLock::new(),
                closed: Signal::default(),
                reading: Mutex::new(()),
            }),
        }
    }

    pub(crate) fn close(&self) {
        self.inner.close();
    }

    pub(crate) async fn next(&self) -> Result<Option<RemoteFrame>, Error> {
        let _read = self
            .inner
            .reading
            .try_lock()
            .map_err(|_| operation(ErrorCode::Conflict, "A frame read is already pending"))?;
        self.inner.result(None)?;
        if self.inner.is_closed() {
            return Ok(None);
        }
        if self.inner.remote.get().is_none() {
            let acquiring = self.inner.origin.observe();
            tokio::pin!(acquiring);
            loop {
                self.inner.result(None)?;
                if self.inner.is_closed() {
                    return Ok(None);
                }
                tokio::select! {
                    _ = self.inner.closed.wait() => return self.inner.result(None),
                    changed = self.inner.origin.changed() => changed?,
                    result = &mut acquiring => {
                        match result {
                            Ok(remote) => {
                                if let Err(remote) = self.inner.remote.set(remote) {
                                    remote.lease.close();
                                    remote.lease.leases_changed.notify_one();
                                    return Err(operation(ErrorCode::Internal, "Frame output was already bound"));
                                }
                                break;
                            }
                            Err(Error::Closed) => {
                                self.inner.close();
                                return self.inner.result(None);
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
        }
        let Some(remote) = self.inner.remote.get() else {
            return Ok(None);
        };
        loop {
            self.inner.result(None)?;
            if self.inner.is_closed() {
                return Ok(None);
            }
            tokio::select! {
                _ = self.inner.closed.wait() => return self.inner.result(None),
                changed = self.inner.origin.changed() => changed?,
                update = remote.next() => match update {
                    MediaStreamUpdate::Frame { frame } => return self.inner.result(Some(frame)),
                    MediaStreamUpdate::Closed => {
                        self.inner.close();
                        return self.inner.result(None);
                    }
                    MediaStreamUpdate::Track { .. } | MediaStreamUpdate::NativeDropped { .. }
                        | MediaStreamUpdate::NativeDecodedDropped { .. } | MediaStreamUpdate::Lagged { .. } => {}
                }
            }
        }
    }
}
