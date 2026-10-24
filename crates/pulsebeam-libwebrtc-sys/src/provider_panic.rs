//! Containment for user code called by native provider adapters.
use std::cell::Cell;
thread_local! { static CALLBACK_DEPTH: Cell<usize> = const { Cell::new(0) }; }

/// Admission marker only. It is not an owner lease and must never be used as
/// evidence that the enclosing SDK caller is retained until its operation ends.
pub(crate) struct ProviderCallbackScope;
impl ProviderCallbackScope {
    pub(crate) fn enter() -> Self {
        CALLBACK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self
    }
}
impl Drop for ProviderCallbackScope {
    fn drop(&mut self) {
        CALLBACK_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}
pub(crate) fn callback_active() -> bool {
    CALLBACK_DEPTH.with(|depth| depth.get() != 0)
}

/// Erase a caught panic without running an untrusted payload destructor.
/// Even catching a second panic from `drop(payload)` is insufficient: its
/// field cleanup can panic during that unwind and abort before the catch.
/// Known string payloads can be freed; arbitrary payloads must be forgotten.
pub(crate) fn catch_provider<T>(call: impl FnOnce() -> T) -> Result<T, ()> {
    let _scope = ProviderCallbackScope::enter();
    catch_task(call)
}

/// Tasks on caller-owned queues are dispatch roots, not native callback loans.
/// They retain panic containment without forbidding ordinary peer control.
/// Current()-snapshot posts may target private SDK queues and use catch_provider.
pub(crate) fn catch_task<T>(call: impl FnOnce() -> T) -> Result<T, ()> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(value) => Ok(value),
        Err(payload) => {
            if payload.is::<String>() || payload.is::<&'static str>() {
                drop(payload);
            } else {
                std::mem::forget(payload);
            }
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{callback_active, catch_provider, catch_task};

    #[test]
    fn callback_admission_restores_nested_scopes_after_panic() {
        assert!(!callback_active());
        catch_task(|| assert!(!callback_active())).unwrap();
        catch_provider(|| {
            assert!(callback_active());
            assert!(catch_provider(|| panic!("nested callback")).is_err());
            assert!(callback_active());
            catch_task(|| assert!(callback_active())).unwrap();
        })
        .unwrap();
        assert!(!callback_active());
        assert!(catch_provider(|| panic!("outer callback")).is_err());
        assert!(!callback_active());
    }

    struct Field;
    impl Drop for Field {
        fn drop(&mut self) {
            panic!("panic during payload field cleanup");
        }
    }
    struct Payload(Field);
    impl Drop for Payload {
        fn drop(&mut self) {
            panic!("panic in payload destructor");
        }
    }

    #[test]
    fn contains_strings_and_payloads_whose_cleanup_would_double_panic() {
        assert_eq!(catch_provider(|| 42), Ok(42));
        assert!(catch_provider(|| panic!("string literal")).is_err());
        assert!(catch_provider(|| std::panic::panic_any(String::from("owned string"))).is_err());
        assert!(catch_provider(|| std::panic::panic_any(Payload(Field))).is_err());
    }
}
