pub mod mpsc;
pub mod slot_group;
pub mod spmc;
pub mod spsc;

pub use mpsc::{
    UnsyncReceiver as UnsyncMpscReceiver, UnsyncSender as UnsyncMpscSender,
    unsync_channel as unsync_mpsc_channel,
};
pub use slot_group::UnsyncSlotGroup;

#[cfg(not(feature = "loom"))]
mod primitives {
    #[cfg(feature = "parking_lot")]
    mod inner {
        pub use parking_lot::{
            MappedMutexGuard, MappedRwLockReadGuard, MappedRwLockWriteGuard, Mutex as RawMutex,
            MutexGuard, RwLock as RawRwLock, RwLockReadGuard, RwLockUpgradableReadGuard,
            RwLockWriteGuard,
        };

        #[derive(Debug)]
        pub struct Mutex<T>(RawMutex<T>);
        impl<T> Mutex<T> {
            pub fn new(val: T) -> Self {
                Self(RawMutex::new(val))
            }
            pub fn lock(&self) -> MutexGuard<'_, T> {
                self.0.lock()
            }
            pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
                self.0.try_lock()
            }
            pub fn get_mut(&mut self) -> &mut T {
                self.0.get_mut()
            }
        }

        #[derive(Debug)]
        pub struct RwLock<T>(RawRwLock<T>);
        impl<T> RwLock<T> {
            pub fn new(val: T) -> Self {
                Self(RawRwLock::new(val))
            }
            pub fn read(&self) -> RwLockReadGuard<'_, T> {
                self.0.read()
            }
            pub fn write(&self) -> RwLockWriteGuard<'_, T> {
                self.0.write()
            }
            pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
                self.0.try_read()
            }
            pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
                self.0.try_write()
            }
            pub fn get_mut(&mut self) -> &mut T {
                self.0.get_mut()
            }
        }
    }

    #[cfg(not(feature = "parking_lot"))]
    mod inner {
        pub use std::sync::{
            Mutex as RawMutex, MutexGuard, RwLock as RawRwLock, RwLockReadGuard, RwLockWriteGuard,
        };

        #[derive(Debug)]
        pub struct Mutex<T>(RawMutex<T>);
        impl<T> Mutex<T> {
            pub fn new(val: T) -> Self {
                Self(RawMutex::new(val))
            }
            pub fn lock(&self) -> MutexGuard<'_, T> {
                self.0.lock().unwrap()
            }
            pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
                self.0.try_lock().ok()
            }
            pub fn get_mut(&mut self) -> &mut T {
                self.0.get_mut().unwrap()
            }
        }

        #[derive(Debug)]
        pub struct RwLock<T>(RawRwLock<T>);
        impl<T> RwLock<T> {
            pub fn new(val: T) -> Self {
                Self(RawRwLock::new(val))
            }
            pub fn read(&self) -> RwLockReadGuard<'_, T> {
                self.0.read().unwrap()
            }
            pub fn write(&self) -> RwLockWriteGuard<'_, T> {
                self.0.write().unwrap()
            }
            pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
                self.0.try_read().ok()
            }
            pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
                self.0.try_write().ok()
            }
            pub fn get_mut(&mut self) -> &mut T {
                self.0.get_mut().unwrap()
            }
        }
    }

    pub use inner::*;
    pub use std::sync::{Barrier, BarrierWaitResult, Condvar, Once, OnceState, WaitTimeoutResult};

    /// Zero-weak-count Arc — drops the weak-count word from every allocation and
    /// the matching atomic acquire on every `clone()`.  This is the canonical
    /// `Arc` for all internal SFU code; import via `crate::sync::Arc` (runtime)
    /// or `pulsebeam_runtime::sync::Arc` (other crates).
    pub use std::sync::Arc;

    pub mod atomic {
        pub use std::sync::atomic::*;
    }

    pub mod coop {
        pub use tokio::task::coop::poll_proceed;
    }

    pub use futures::task::AtomicWaker;
}

#[cfg(feature = "loom")]
mod primitives {
    mod inner {
        pub use loom::sync::{
            Mutex as RawMutex, MutexGuard, RwLock as RawRwLock, RwLockReadGuard, RwLockWriteGuard,
        };

        #[derive(Debug)]
        pub struct Mutex<T>(RawMutex<T>);
        impl<T> Mutex<T> {
            pub fn new(val: T) -> Self {
                Self(RawMutex::new(val))
            }
            pub fn lock(&self) -> MutexGuard<'_, T> {
                self.0.lock().unwrap()
            }
            pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
                self.0.try_lock().ok()
            }
        }

        #[derive(Debug)]
        pub struct RwLock<T>(RawRwLock<T>);
        impl<T> RwLock<T> {
            pub fn new(val: T) -> Self {
                Self(RawRwLock::new(val))
            }
            pub fn read(&self) -> RwLockReadGuard<'_, T> {
                self.0.read().unwrap()
            }
            pub fn write(&self) -> RwLockWriteGuard<'_, T> {
                self.0.write().unwrap()
            }
        }
    }

    pub use inner::*;
    #[derive(Debug)]
    pub struct Arc<T: ?Sized>(loom::sync::Arc<T>);

    impl<T> Arc<T> {
        pub fn new(data: T) -> Self {
            Self(loom::sync::Arc::new(data))
        }
    }

    impl<T: ?Sized> Clone for Arc<T> {
        fn clone(&self) -> Self {
            Self(self.0.clone())
        }
    }

    impl<T: ?Sized> std::ops::Deref for Arc<T> {
        type Target = T;
        fn deref(&self) -> &Self::Target {
            self.0.deref()
        }
    }

    impl<T: ?Sized + PartialEq> PartialEq for Arc<T> {
        fn eq(&self, other: &Self) -> bool {
            **self == **other
        }
    }

    impl<T: ?Sized + Eq> Eq for Arc<T> {}

    impl<T: ?Sized + PartialOrd> PartialOrd for Arc<T> {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            (**self).partial_cmp(&**other)
        }
    }

    impl<T: ?Sized + Ord> Ord for Arc<T> {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            (**self).cmp(&**other)
        }
    }

    impl<T: ?Sized + std::hash::Hash> std::hash::Hash for Arc<T> {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            (**self).hash(state)
        }
    }

    impl<T: Default> Default for Arc<T> {
        fn default() -> Self {
            Self::new(T::default())
        }
    }

    impl<T: ?Sized + std::fmt::Display> std::fmt::Display for Arc<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Display::fmt(&**self, f)
        }
    }

    impl<T: ?Sized> Arc<T> {
        pub fn as_ptr(this: &Self) -> *const T {
            &*this.0 as *const T
        }

        pub fn into_raw(this: Self) -> *const T {
            loom::sync::Arc::into_raw(this.0)
        }

        pub unsafe fn from_raw(ptr: *const T) -> Self {
            Self(loom::sync::Arc::from_raw(ptr))
        }

        pub unsafe fn increment_strong_count(ptr: *const T) {
            loom::sync::Arc::increment_strong_count(ptr)
        }

        pub unsafe fn decrement_strong_count(ptr: *const T) {
            loom::sync::Arc::decrement_strong_count(ptr)
        }

        pub fn ptr_eq(this: &Self, other: &Self) -> bool {
            loom::sync::Arc::get_mut(&mut this.0.clone()).is_none() && // This is a hack, loom doesn't have ptr_eq directly sometimes
            Arc::as_ptr(this) == Arc::as_ptr(other)
        }
    }

    pub mod atomic {
        pub use loom::sync::atomic::*;
    }

    pub mod coop {
        use std::task::{Context, Poll};

        pub struct CoopToken;

        impl CoopToken {
            pub fn made_progress(self) {}
        }

        pub fn poll_proceed(_cx: &mut Context<'_>) -> Poll<CoopToken> {
            Poll::Ready(CoopToken)
        }
    }

    pub struct AtomicWaker(loom::future::AtomicWaker);

    impl AtomicWaker {
        pub fn new() -> Self {
            Self(loom::future::AtomicWaker::new())
        }

        pub fn register(&self, waker: &std::task::Waker) {
            self.0.register_by_ref(waker); // loom uses register_by_ref
        }

        pub fn wake(&self) {
            self.0.wake();
        }
    }

    impl Default for AtomicWaker {
        fn default() -> Self {
            Self::new()
        }
    }
}

pub use primitives::*;
