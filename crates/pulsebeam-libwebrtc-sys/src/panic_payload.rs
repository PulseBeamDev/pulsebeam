// A caught panic payload must not unwind through a native callback boundary.
// Standard string payloads have non-panicking destructors. Unknown application
// payloads may panic while being dropped, so preserve the conservative fallback
// only for those types rather than leaking every ordinary panic allocation.
pub(crate) fn discard(payload: Box<dyn std::any::Any + Send>) {
    if payload.is::<String>() || payload.is::<&'static str>() {
        drop(payload);
    } else {
        std::mem::forget(payload);
    }
}
