extern crate alloc;

mod host;
pub mod watch;
pub mod mpsc;
mod web_agent;

pub use web_agent::*;

#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    host::install();
}
