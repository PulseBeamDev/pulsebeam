use tokio::sync::mpsc;

pub fn bounded<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    debug_assert!(capacity > 0);
    let (sender, receiver) = mpsc::channel(capacity);
    (Sender { inner: sender }, Receiver { inner: receiver })
}

#[derive(Clone)]
pub struct Sender<T> {
    inner: mpsc::Sender<T>,
}

pub struct Receiver<T> {
    inner: mpsc::Receiver<T>,
}

impl<T> Sender<T> {
    pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
        self.inner
            .send(value)
            .await
            .map_err(|error| SendError(error.0))
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.inner.try_send(value).map_err(|error| match error {
            mpsc::error::TrySendError::Full(value) => TrySendError::Full(value),
            mpsc::error::TrySendError::Closed(value) => TrySendError::Closed(value),
        })
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        self.inner.recv().await.ok_or(RecvError)
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.inner.try_recv().map_err(|error| match error {
            mpsc::error::TryRecvError::Empty => TryRecvError::Empty,
            mpsc::error::TryRecvError::Disconnected => TryRecvError::Disconnected,
        })
    }
}

#[derive(Debug)]
pub struct SendError<T>(pub T);

impl<T> std::fmt::Display for SendError<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("mailbox closed")
    }
}

impl<T: std::fmt::Debug> std::error::Error for SendError<T> {}

#[derive(Debug)]
pub enum TrySendError<T> {
    Full(T),
    Closed(T),
}

impl<T> std::fmt::Display for TrySendError<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(_) => formatter.write_str("mailbox full"),
            Self::Closed(_) => formatter.write_str("mailbox closed"),
        }
    }
}

impl<T: std::fmt::Debug> std::error::Error for TrySendError<T> {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecvError;

impl std::fmt::Display for RecvError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("mailbox closed")
    }
}

impl std::error::Error for RecvError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

impl std::fmt::Display for TryRecvError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("mailbox empty"),
            Self::Disconnected => formatter.write_str("mailbox disconnected"),
        }
    }
}

impl std::error::Error for TryRecvError {}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_mailbox_preserves_fifo() {
        let (sender, mut receiver) = bounded(2);
        sender.send(1).await.unwrap();
        sender.send(2).await.unwrap();
        assert_eq!(receiver.recv().await.unwrap(), 1);
        assert_eq!(receiver.recv().await.unwrap(), 2);
    }
}
