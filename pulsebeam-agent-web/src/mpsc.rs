//! Single-threaded, multi-producer FIFO channel.

use alloc::{collections::VecDeque, rc::Rc};
use core::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use spin::Mutex;

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Rc::new(Shared {
        state: Mutex::new(State {
            values: VecDeque::new(),
            waker: None,
            sender_alive: true,
            receiver_alive: true,
        }),
    });
    (
        Sender {
            shared: Rc::clone(&shared),
        },
        Receiver { shared },
    )
}

struct Shared<T> {
    state: Mutex<State<T>>,
}

struct State<T> {
    values: VecDeque<T>,
    waker: Option<Waker>,
    sender_alive: bool,
    receiver_alive: bool,
}

#[derive(Clone)]
pub struct Sender<T> {
    shared: Rc<Shared<T>>,
}
pub struct Receiver<T> {
    shared: Rc<Shared<T>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;
impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("channel closed")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SendError<T>(T);
impl<T> SendError<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}
impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("channel receiver dropped")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Closed,
}

impl<T> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        let waker = {
            let mut state = self.shared.state.lock();
            if !state.receiver_alive {
                return Err(SendError(value));
            }
            state.values.push_back(value);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }
    pub fn is_closed(&self) -> bool {
        !self.shared.state.lock().receiver_alive
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if Rc::strong_count(&self.shared) != 2 {
            return;
        }
        let waker = {
            let mut state = self.shared.state.lock();
            state.sender_alive = false;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Receiver<T> {
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv {
            receiver: self,
            done: false,
        }
    }
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut state = self.shared.state.lock();
        if let Some(value) = state.values.pop_front() {
            return Ok(value);
        }
        if state.sender_alive {
            Err(TryRecvError::Empty)
        } else {
            Err(TryRecvError::Closed)
        }
    }
    pub fn is_closed(&self) -> bool {
        !self.shared.state.lock().sender_alive
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let (values, waker) = {
            let mut state = self.shared.state.lock();
            state.receiver_alive = false;
            (core::mem::take(&mut state.values), state.waker.take())
        };
        drop(values);
        drop(waker);
    }
}

pub struct Recv<'a, T> {
    receiver: &'a mut Receiver<T>,
    done: bool,
}
impl<T> Future for Recv<'_, T> {
    type Output = Result<T, Closed>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        debug_assert!(!self.done, "Recv polled after completion");
        if self.done {
            return Poll::Pending;
        }
        let this = &mut *self;
        let (result, old_waker) = {
            let mut state = this.receiver.shared.state.lock();
            if let Some(value) = state.values.pop_front() {
                (Some(Ok(value)), state.waker.take())
            } else if !state.sender_alive {
                (Some(Err(Closed)), state.waker.take())
            } else {
                let replace = state
                    .waker
                    .as_ref()
                    .is_none_or(|waker| !waker.will_wake(cx.waker()));
                (
                    None,
                    replace
                        .then(|| state.waker.replace(cx.waker().clone()))
                        .flatten(),
                )
            }
        };
        drop(old_waker);
        match result {
            Some(result) => {
                this.done = true;
                Poll::Ready(result)
            }
            None => Poll::Pending,
        }
    }
}
impl<T> Drop for Recv<'_, T> {
    fn drop(&mut self) {
        if !self.done {
            drop(self.receiver.shared.state.lock().waker.take());
        }
    }
}
