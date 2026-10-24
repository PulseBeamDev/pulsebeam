//! Private owning queue leaf retained by the native APM attachment.
use crate::{AudioProcessorError, TaskQueue, ffi};

pub(crate) struct RustApmDumpWorker {
    queue: TaskQueue,
}
impl RustApmDumpWorker {
    pub(crate) fn new(queue: TaskQueue) -> Result<Box<Self>, AudioProcessorError> {
        if queue.native_for_consumer().is_none() {
            return Err(AudioProcessorError::InvalidDumpWorker);
        }
        Ok(Box::new(Self { queue }))
    }
}
pub(crate) fn apm_dump_worker_queue(worker: &RustApmDumpWorker) -> &ffi::NativeTaskQueue {
    // Exclusive ownership moved into this private leaf after validation. No
    // caller can close the queue between the check and native attachment.
    worker
        .queue
        .native_for_consumer()
        .expect("validated dump worker")
}
