/** @internal Package-to-package bridge for @pulsebeam/react, not an application API. */
export { createManagedAgent } from "../agent.js";
export { nativeSourceTrack, watchSource } from "../capture-source.js";
export { attachMedia } from "../media-attachment.js";
export type { BorrowedMediaAttachment } from "../media-attachment.js";
export {
  attachRemoteAudio,
  attachRemoteVideo,
  validateReceiveOptions,
} from "../remote-catalog.js";
export type { ManagedAgent } from "../agent.js";
