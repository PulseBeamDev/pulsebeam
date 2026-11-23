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
  AudioPublication,
  AudioSource,
  AudioSubscription,
  AudioSubscriptionUpdates,
  Catalog,
  CatalogParticipant,
  CatalogUpdates,
  CatalogWatchState as WatchState,
  DataPublication,
  DataPublicationUpdates,
  DataSubscription,
  DataSubscriptionUpdates,
  LocalAudioTrack,
  LocalDataTrack,
  LocalVideoTrack,
  Participant,
  Pipe,
  PipeUpdates,
  PublicationUpdates,
  RemoteVideoTrack,
  RemoteAudioTrack,
  RemoteDataTrack,
  RoomRef,
  VideoPublication,
  VideoSource,
  VideoSubscription,
  VideoSubscriptionUpdates,
  WebAgentConfig as AgentConfig,
  WebAgentError as AgentError,
  WebAudioOutput,
  WebCatalogPage,
  WebVideoOutput,
} from "./generated/bindings/pulsebeam_agent_web.js";
export {
  AgentState,
  AgentStatus,
  AudioSubscriptionConfig,
  AudioSubscriptionState,
  DataPublicationState,
  DataSubscriptionState,
  ErrorCode,
  ErrorInfo,
  PipeState,
  PipeStatus,
  PublicationAdmissionStatus,
  PublicationConfig,
  PublicationState,
  VideoSubscriptionState,
  WatchConfig,
  WatchStatus,
} from "./generated/bindings/pulsebeam_agent_core.js";
