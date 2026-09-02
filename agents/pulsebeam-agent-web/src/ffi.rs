#![allow(
    clippy::disallowed_types,
    reason = "UniFFI requires Arc ownership for exported object facades"
)]

use std::{str::FromStr, sync::Arc};

use js_sys::{BigInt, Function, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{MediaStream, MediaStreamTrack};

use agent_core::ffi::AgentConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebMediaTrack(pub u64);

uniffi::custom_newtype!(WebMediaTrack, u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebMediaStream(pub u64);

uniffi::custom_newtype!(WebMediaStream, u64);

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum BindingProofError {
    #[error("invalid core configuration: {message}")]
    InvalidConfiguration { message: String },
    #[error("browser media registry is unavailable")]
    RegistryUnavailable,
    #[error("browser media handle is stale or belongs to another media kind")]
    InvalidMediaHandle,
    #[error("browser media handle space is exhausted")]
    HandleExhausted,
    #[error("browser media registry rejected an operation: {message}")]
    RegistryFailure { message: String },
}

#[derive(uniffi::Object)]
pub struct MediaRegistryProof;

#[uniffi::export]
impl MediaRegistryProof {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    pub fn round_trip_track(
        &self,
        track: WebMediaTrack,
    ) -> Result<WebMediaTrack, BindingProofError> {
        let _: MediaStreamTrack = registry_value(track.0, "track")?
            .dyn_into()
            .map_err(|_| BindingProofError::InvalidMediaHandle)?;
        Ok(track)
    }

    pub fn create_stream(&self) -> Result<WebMediaStream, BindingProofError> {
        let stream = MediaStream::new().map_err(registry_failure)?;
        retain(stream.into(), "stream").map(WebMediaStream)
    }

    pub fn round_trip_stream(
        &self,
        stream: WebMediaStream,
    ) -> Result<WebMediaStream, BindingProofError> {
        let _: MediaStream = registry_value(stream.0, "stream")?
            .dyn_into()
            .map_err(|_| BindingProofError::InvalidMediaHandle)?;
        Ok(stream)
    }

    pub fn release_track(&self, track: WebMediaTrack) -> Result<(), BindingProofError> {
        release(track.0, "track")
    }

    pub fn release_stream(&self, stream: WebMediaStream) -> Result<(), BindingProofError> {
        release(stream.0, "stream")
    }

    pub fn retained_media(&self) -> Result<u64, BindingProofError> {
        bigint_u64(call_registry("size", &[])?)
    }
}

#[uniffi::export]
pub fn normalize_agent_config(config: AgentConfig) -> Result<AgentConfig, BindingProofError> {
    config
        .into_core()
        .map(Into::into)
        .map_err(|error| BindingProofError::InvalidConfiguration {
            message: error.message,
        })
}

fn retain(value: JsValue, kind: &str) -> Result<u64, BindingProofError> {
    let result = call_registry("retain", &[value, JsValue::from_str(kind)])?;
    bigint_u64(result)
}

fn registry_value(handle: u64, kind: &str) -> Result<JsValue, BindingProofError> {
    let result = call_registry(
        "get",
        &[BigInt::from(handle).into(), JsValue::from_str(kind)],
    )?;
    if result.is_null() || result.is_undefined() {
        Err(BindingProofError::InvalidMediaHandle)
    } else {
        Ok(result)
    }
}

fn release(handle: u64, kind: &str) -> Result<(), BindingProofError> {
    let released = call_registry(
        "release",
        &[BigInt::from(handle).into(), JsValue::from_str(kind)],
    )?;
    match released.as_bool() {
        Some(true) => Ok(()),
        Some(false) => Err(BindingProofError::InvalidMediaHandle),
        None => Err(BindingProofError::RegistryFailure {
            message: "registry returned an invalid release result".to_owned(),
        }),
    }
}

fn call_registry(name: &str, arguments: &[JsValue]) -> Result<JsValue, BindingProofError> {
    let registry = Reflect::get(
        &js_sys::global(),
        &JsValue::from_str("__pulsebeamMediaRegistry"),
    )
    .map_err(registry_failure)?;
    if registry.is_null() || registry.is_undefined() {
        return Err(BindingProofError::RegistryUnavailable);
    }
    let function = Reflect::get(&registry, &JsValue::from_str(name))
        .map_err(registry_failure)?
        .dyn_into::<Function>()
        .map_err(|_| BindingProofError::RegistryUnavailable)?;
    match arguments {
        [] => function.call0(&registry),
        [first] => function.call1(&registry, first),
        [first, second] => function.call2(&registry, first, second),
        _ => {
            return Err(BindingProofError::RegistryFailure {
                message: "media registry call exceeded its fixed arity".to_owned(),
            });
        }
    }
    .map_err(registry_failure)
}

fn bigint_u64(value: JsValue) -> Result<u64, BindingProofError> {
    let bigint = BigInt::new(&value).map_err(|error| registry_failure(error.into()))?;
    let text = bigint
        .to_string(10)
        .map_err(|error| registry_failure(error.into()))?
        .as_string()
        .ok_or_else(|| BindingProofError::RegistryFailure {
            message: "registry returned a non-numeric handle".to_owned(),
        })?;
    u64::from_str(&text).map_err(|_| BindingProofError::HandleExhausted)
}

fn registry_failure(value: JsValue) -> BindingProofError {
    let message = value
        .dyn_ref::<js_sys::Error>()
        .and_then(|error| error.message().as_string())
        .or_else(|| value.as_string())
        .unwrap_or_else(|| "unknown JavaScript failure".to_owned());
    if message.contains("exhausted") {
        BindingProofError::HandleExhausted
    } else {
        BindingProofError::RegistryFailure { message }
    }
}
