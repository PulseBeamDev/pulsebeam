/** @internal Package-to-package bridge for @pulsebeam/react, not an application API. */
export { createManagedAgent } from "../agent.js";
export { nativeSourceTrack } from "../capture-source.js";
export { claimPublication } from "../media.js";
export {
  attachRemoteAudio,
  attachRemoteVideo,
  validateReceiveOptions,
} from "../remote-catalog.js";
export type { ManagedAgent } from "../agent.js";
