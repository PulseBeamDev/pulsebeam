use super::{Arc, ErrorCode, ErrorInfo, WebAgentError, video::RoomRef};
use agent_core::ffi;

#[uniffi::export]
impl RoomRef {
    #[uniffi::method(default(options = None))]
    pub fn watch(&self, options: Option<ffi::WatchConfig>) -> Result<Arc<Catalog>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let runtime = self
                .agent
                .runtime
                .open_watch(self.room.clone(), options.unwrap_or_default())?;
            Ok(Arc::new(Catalog { runtime }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = options;
            Err(super::browser_required())
        }
    }
}

#[derive(uniffi::Object)]
pub struct Catalog {
    #[cfg(target_arch = "wasm32")]
    runtime: crate::browser::catalog::BrowserCatalogWatch,
}

#[uniffi::export]
impl Catalog {
    pub fn state(&self) -> Result<ffi::WatchState, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            Ok(self.runtime.current_state())
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(super::browser_required())
    }

    pub fn updates(&self) -> Result<Arc<CatalogUpdates>, WebAgentError> {
        #[cfg(target_arch = "wasm32")]
        {
            let cursor = self.runtime.open_updates()?;
            Ok(Arc::new(CatalogUpdates { cursor }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err(super::browser_required())
    }

    pub async fn close(&self) {
        #[cfg(target_arch = "wasm32")]
        self.runtime.close();
    }
}

state_cursor!(CatalogUpdates, WatchState, WatchUpdates);
