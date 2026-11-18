use super::*;

pub(super) fn observer_limit(_: agent_core::observer::ObserverLimit) -> NativeError {
    NativeError::Operation {
        info: core_ffi::ErrorInfo::new(
            core_ffi::ErrorCode::CapacityExceeded,
            "Resource observation cursor capacity exceeded",
        ),
    }
}

macro_rules! updates {
    ($name:ident, $state:ident) => {
        #[derive(uniffi::Object)]
        pub struct $name {
            pub(super) runtime: agent_core::observer::$name,
        }

        #[uniffi::export(async_runtime = "tokio")]
        impl $name {
            pub async fn next(&self) -> Result<Option<core_ffi::$state>, NativeError> {
                let next = self.runtime.next().map_err(|_| NativeError::Operation {
                    info: core_ffi::ErrorInfo::new(
                        core_ffi::ErrorCode::Conflict,
                        "An observation read is already pending",
                    ),
                })?;
                Ok(next.await)
            }

            pub fn close(&self) {
                self.runtime.close();
            }
        }
    };
}

updates!(WatchUpdates, WatchState);
updates!(VideoSubscriptionUpdates, VideoSubscriptionState);
updates!(AudioSubscriptionUpdates, AudioSubscriptionState);
updates!(DataSubscriptionUpdates, DataSubscriptionState);
updates!(DataPublicationUpdates, DataPublicationState);
