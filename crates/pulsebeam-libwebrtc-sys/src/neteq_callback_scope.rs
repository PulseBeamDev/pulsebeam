//! Reject same-thread callback recursion before taking a provider mutex.
use std::cell::RefCell;

thread_local! {
    static ACTIVE: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct NetEqCallbackScope(usize);
impl NetEqCallbackScope {
    pub(crate) fn enter(identity: usize) -> Option<Self> {
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if active.contains(&identity) {
                None
            } else {
                active.push(identity);
                Some(Self(identity))
            }
        })
    }
}
impl Drop for NetEqCallbackScope {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            let removed = active.borrow_mut().pop();
            debug_assert_eq!(removed, Some(self.0));
        });
    }
}
