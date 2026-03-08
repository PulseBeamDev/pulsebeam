pub mod bit_signal;
pub mod mpsc;
pub mod pool_buf;
pub mod spmc;
pub mod task_group;

pub use pool_buf::{BufPool, PoolBuf, PoolBufMut, MAX_PAYLOAD as POOL_MAX_PAYLOAD};

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
    pub use triomphe::Arc;

    pub mod atomic {
        pub use std::sync::atomic::*;
    }
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
    pub use std::sync::Arc;

    pub mod atomic {
        pub use loom::sync::atomic::*;
    }
}

pub use primitives::*;
