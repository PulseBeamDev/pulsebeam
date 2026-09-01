#![no_std]

extern crate alloc;

mod watch;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub fn start() {}

#[wasm_bindgen]
pub fn greet(_name: &str) {}
