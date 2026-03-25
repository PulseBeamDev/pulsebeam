use std::{
    cell::RefCell,
    collections::BTreeMap,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

pub struct Event {
    inner: Rc<RefCell<Inner>>,
}

struct Inner {
    listeners: BTreeMap<usize, ListenerEntry>,
    next_id: usize,
}

struct ListenerEntry {
    waker: Option<Waker>,
    notified: bool,
}

impl Event {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(Inner {
                listeners: BTreeMap::new(),
                next_id: 0,
            })),
        }
    }

    pub fn listen(&self) -> EventListener {
        let mut inner = self.inner.borrow_mut();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.listeners.insert(
            id,
            ListenerEntry {
                waker: None,
                notified: false,
            },
        );

        EventListener {
            event: Rc::clone(&self.inner),
            id,
        }
    }

    pub fn notify(&self, n: usize) {
        let mut inner = self.inner.borrow_mut();
        let mut count = 0;

        for entry in inner.listeners.values_mut() {
            if count >= n {
                break;
            }
            if entry.notified {
                continue;
            }

            entry.notified = true;
            if let Some(waker) = &entry.waker {
                // Hot path optimization: no atomic inc/dec
                waker.wake_by_ref();
            }
            count += 1;
        }
    }

    pub fn notify_all(&self) {
        self.notify(usize::MAX);
    }
}

pub struct EventListener {
    event: Rc<RefCell<Inner>>,
    id: usize,
}

impl Drop for EventListener {
    fn drop(&mut self) {
        // Manual unregistration
        self.event.borrow_mut().listeners.remove(&self.id);
    }
}

impl std::future::Future for EventListener {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.event.borrow_mut();
        let entry = inner.listeners.get_mut(&self.id).expect("Listener leaked");

        if entry.notified {
            return Poll::Ready(());
        }

        // Update waker if it changed or is missing
        if entry
            .waker
            .as_ref()
            .map_or(true, |w| !w.will_wake(cx.waker()))
        {
            entry.waker = Some(cx.waker().clone());
        }

        Poll::Pending
    }
}
