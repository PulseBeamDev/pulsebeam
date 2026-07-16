use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SendError<T>(pub T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendError").finish_non_exhaustive()
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sending on a closed channel")
    }
}

impl<T> std::error::Error for SendError<T> {}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.debug_tuple("Full").finish(),
            Self::Disconnected(_) => f.debug_tuple("Disconnected").finish(),
        }
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrySendError::Full(_) => write!(f, "sending on a full channel"),
            TrySendError::Disconnected(_) => write!(f, "sending on a closed channel"),
        }
    }
}

impl<T> std::error::Error for TrySendError<T> {}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvError {
    Disconnected,
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvError::Disconnected => "receiving on a closed channel".fmt(f),
        }
    }
}

impl std::error::Error for RecvError {}

#[derive(Clone)]
pub struct Sender<T: Clone> {
    inner: flume::Sender<T>,
}

impl<T: Clone> Sender<T> {
    pub async fn send(&self, msg: T) -> Result<(), SendError<T>> {
        self.inner
            .send_async(msg)
            .await
            .map_err(|flume::SendError(rejected_msg)| SendError(rejected_msg))
    }

    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.inner.try_send(msg).map_err(|e| match e {
            flume::TrySendError::Full(rejected_msg) => TrySendError::Full(rejected_msg),
            flume::TrySendError::Disconnected(rejected_msg) => {
                TrySendError::Disconnected(rejected_msg)
            }
        })
    }
}

#[derive(Clone)]
pub struct Receiver<T> {
    inner: flume::Receiver<T>,
}

impl<T> Receiver<T> {
    pub async fn recv(&self) -> Result<T, RecvError> {
        self.inner
            .recv_async()
            .await
            .map_err(|_| RecvError::Disconnected)
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.inner.try_recv().map_err(|e| match e {
            flume::TryRecvError::Empty => TryRecvError::Empty,
            flume::TryRecvError::Disconnected => TryRecvError::Disconnected,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

pub fn bounded<T: Clone>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = flume::bounded(cap);
    (Sender { inner: tx }, Receiver { inner: rx })
}
