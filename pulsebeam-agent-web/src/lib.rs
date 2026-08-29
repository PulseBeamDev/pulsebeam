#![no_std]

extern crate alloc;

mod host;
mod watch;
mod web_agent;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    pub fn alert(s: &str);
}

#[wasm_bindgen(start)]
pub fn start() {
    host::install();
}

#[wasm_bindgen]
pub fn greet(name: &str) {}
