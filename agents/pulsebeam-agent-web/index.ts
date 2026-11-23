import core from "./generated/bindings/pulsebeam_agent_core.js";
import web from "./generated/bindings/pulsebeam_agent_web.js";
import { initSync } from "./generated/bindings/wasm-bindgen/index.js";
import { wasmModule } from "./web/wasm-bytes.js";

initSync({ module: wasmModule() });
core.initialize();
web.initialize();

export {
  Agent,
  AgentUpdates,
  Participant,
  RemoteVideoTrack,
  RoomRef,
  VideoSubscription,
  VideoSubscriptionUpdates,
  WebAgentConfig as AgentConfig,
  WebAgentError as AgentError,
  WebVideoOutput,
} from "./generated/bindings/pulsebeam_agent_web.js";
export {
  AgentState,
  AgentStatus,
  ErrorCode,
  ErrorInfo,
  VideoSubscriptionState,
} from "./generated/bindings/pulsebeam_agent_core.js";
