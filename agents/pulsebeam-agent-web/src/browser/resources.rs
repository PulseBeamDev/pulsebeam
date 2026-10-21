use agent_core::{
    AgentError, DataEvent, ResourceCommand, ResourceResult, TrackResourceError, TrackSpec,
    VideoObservation,
};
use js_sys::{Array, BigInt, Object};
use serde::Deserialize;
use wasm_bindgen::JsValue;

use super::{PlayoutDelayConfig, TopicModeConfig, TrackSelectorConfig, js_error, set};

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum SpecConfig {
    Video {
        label: String,
    },
    Audio {
        label: String,
    },
    Data {
        label: String,
        delivery: TopicModeConfig,
    },
}

impl From<SpecConfig> for TrackSpec {
    fn from(value: SpecConfig) -> Self {
        match value {
            SpecConfig::Video { label } => Self::Video { label },
            SpecConfig::Audio { label } => Self::Audio { label },
            SpecConfig::Data { label, delivery } => Self::Data {
                label,
                delivery: delivery.into(),
            },
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct ObservationConfig {
    visible: bool,
    height: u32,
    min_height: u32,
    min_fps: u32,
    priority: u32,
    playout_delay: PlayoutDelayConfig,
}

impl From<ObservationConfig> for VideoObservation {
    fn from(value: ObservationConfig) -> Self {
        Self {
            visible: value.visible,
            height: value.height,
            min_height: value.min_height,
            min_fps: value.min_fps,
            priority: value.priority,
            playout_delay: value.playout_delay.into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "op",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum CommandConfig {
    ClaimPublication {
        spec: SpecConfig,
    },
    UpdatePublication {
        owner: u64,
        present: bool,
    },
    ClaimVideo {
        selector: TrackSelectorConfig,
    },
    ObserveVideo {
        owner: u64,
        observation: ObservationConfig,
    },
    ClaimAudio,
    RetainProducer {
        spec: SpecConfig,
    },
    ReceiveData {
        spec: SpecConfig,
        participant_id: Option<String>,
    },
    ReadData {
        owner: u64,
    },
    Release {
        owner: u64,
    },
}

pub(super) fn spec(value: JsValue) -> Result<TrackSpec, JsValue> {
    let config: SpecConfig = serde_wasm_bindgen::from_value(value)
        .map_err(|error| js_error(format!("invalid track descriptor: {error}")))?;
    Ok(config.into())
}

pub(super) fn command(value: JsValue) -> Result<ResourceCommand, JsValue> {
    let config: CommandConfig = serde_wasm_bindgen::from_value(value)
        .map_err(|error| js_error(format!("invalid resource command: {error}")))?;
    Ok(match config {
        CommandConfig::ClaimPublication { spec } => {
            ResourceCommand::ClaimPublication { spec: spec.into() }
        }
        CommandConfig::UpdatePublication { owner, present } => {
            ResourceCommand::UpdatePublication { owner, present }
        }
        CommandConfig::ClaimVideo { selector } => ResourceCommand::ClaimVideo {
            selector: selector.into(),
        },
        CommandConfig::ObserveVideo { owner, observation } => ResourceCommand::ObserveVideo {
            owner,
            observation: observation.into(),
        },
        CommandConfig::ClaimAudio => ResourceCommand::ClaimAudio,
        CommandConfig::RetainProducer { spec } => {
            ResourceCommand::RetainProducer { spec: spec.into() }
        }
        CommandConfig::ReceiveData {
            spec,
            participant_id,
        } => ResourceCommand::ReceiveData {
            spec: spec.into(),
            participant_id,
        },
        CommandConfig::ReadData { owner } => ResourceCommand::ReadData { owner },
        CommandConfig::Release { owner } => ResourceCommand::Release { owner },
    })
}

pub(super) fn result(value: ResourceResult) -> JsValue {
    match value {
        ResourceResult::Publication { owner, slot, .. } => {
            let value = Object::new();
            set(&value, "owner", BigInt::from(owner));
            set(&value, "slot", slot);
            value.into()
        }
        ResourceResult::Owned { owner } => {
            let value = Object::new();
            set(&value, "owner", BigInt::from(owner));
            value.into()
        }
        ResourceResult::Event { event: None } => JsValue::NULL,
        ResourceResult::Event { event: Some(event) } => data_event(event),
        ResourceResult::Updated => JsValue::UNDEFINED,
    }
}

fn data_event(event: DataEvent) -> JsValue {
    let value = Object::new();
    match event {
        DataEvent::Message {
            participant_id,
            track_id,
            sequence,
            data,
        } => {
            set(&value, "type", "message");
            set(&value, "participantId", participant_id);
            set(&value, "trackId", track_id);
            set(&value, "sequence", BigInt::from(sequence));
            set(&value, "data", js_sys::Uint8Array::from(data.as_slice()));
        }
        DataEvent::Gap {
            participant_id,
            track_id,
            from,
            to,
        } => {
            set(&value, "type", "gap");
            set(&value, "participantId", participant_id);
            set(&value, "trackId", track_id);
            set(&value, "from", BigInt::from(from));
            set(&value, "to", BigInt::from(to));
        }
    }
    value.into()
}

pub(super) fn ready(owners: &[u64]) -> Object {
    let value = Object::new();
    set(&value, "type", "data-ready");
    let ids = Array::new();
    for owner in owners {
        ids.push(&BigInt::from(*owner));
    }
    set(&value, "owners", ids);
    value
}

pub(super) fn error(error: AgentError) -> JsValue {
    let code = match &error {
        AgentError::InvalidConfiguration(agent_core::ValidationError::TopicChannelLimit {
            ..
        })
        | AgentError::InvalidResource(TrackResourceError::InvalidSpec(
            agent_core::ValidationError::TopicChannelLimit { .. },
        )) => "capacity",
        AgentError::InvalidResource(resource) => match resource {
            TrackResourceError::InvalidSpec(_)
            | TrackResourceError::WrongKind
            | TrackResourceError::InvalidPlayoutDelay => "validation",
            TrackResourceError::NotConnected => "not-connected",
            TrackResourceError::AlreadyOwned | TrackResourceError::OwnerInUse => "ownership",
            TrackResourceError::OwnerSpaceExhausted => "owner-exhausted",
            TrackResourceError::Capacity { .. } => "capacity",
            TrackResourceError::Released => "released",
            TrackResourceError::ReceiveOverflow => "receive-overflow",
            TrackResourceError::ReceiveChannelFailed(_) => "receive-channel-failed",
        },
        AgentError::InvalidTopic(agent_core::TopicError::PayloadTooLarge { .. }) => {
            "payload-too-large"
        }
        AgentError::InvalidTopic(agent_core::TopicError::SendQueueFull(_)) => "capacity",
        AgentError::InvalidTopic(agent_core::TopicError::InvalidTopic(_)) => "validation",
        AgentError::InvalidTopic(agent_core::TopicError::PublisherUnavailable(_)) => "unavailable",
        AgentError::InvalidConfiguration(_) => "validation",
        _ => "runtime",
    };
    coded_error(code, &error.to_string())
}

pub(super) fn send_error(error: crate::engine::DataSendError) -> JsValue {
    use crate::engine::DataSendError;
    use agent_core::DataSendFailure;
    let message = error.to_string();
    let code = match error {
        DataSendError::Admission(error) => return self::error(error),
        DataSendError::Closed => "closed",
        DataSendError::Delivery(failure) => match failure {
            DataSendFailure::Disconnected => "not-connected",
            DataSendFailure::TerminalFailure => "terminal-failure",
            DataSendFailure::TimedOut => "send-timeout",
            DataSendFailure::TransportReplaced => "stale-binding",
            DataSendFailure::ChannelFailed => "send-channel-failed",
            DataSendFailure::Superseded => "superseded",
            DataSendFailure::HostRejected => "host-rejected",
            DataSendFailure::QueueFull => "capacity",
            DataSendFailure::SequenceExhausted => "sequence-exhausted",
        },
    };
    coded_error(code, &message)
}

fn coded_error(code: &str, message: &str) -> JsValue {
    let value = js_sys::Error::new(message);
    set(value.as_ref(), "pulsebeamCode", code);
    value.into()
}
