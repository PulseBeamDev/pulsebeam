#[allow(unused_imports)]
use uniffi_runtime_javascript::{self as js, uniffi as u, IntoJs, IntoRust};
use wasm_bindgen::prelude::wasm_bindgen;
extern "C" {
    fn ffi_pulsebeam_agent_core_uniffi_contract_version() -> u32;
}
#[wasm_bindgen]
pub unsafe fn ubrn_ffi_pulsebeam_agent_core_uniffi_contract_version() -> js::UInt32 {
    ffi_pulsebeam_agent_core_uniffi_contract_version().into_js()
}
