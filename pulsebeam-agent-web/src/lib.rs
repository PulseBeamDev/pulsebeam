extern crate alloc;

#[cfg(target_arch = "wasm32")]
mod host;
pub mod watch;
mod web_agent;

pub use web_agent::*;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    host::install();
}
