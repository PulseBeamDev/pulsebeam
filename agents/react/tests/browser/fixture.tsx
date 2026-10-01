import * as api from "@pulsebeam/react";
import { runAcquisitionContract } from "./acquisition.js";
import { runConcurrencyContract } from "./concurrency.js";
import { runOwnershipContract } from "./ownership.js";
import { runAutoplayContract, runPlaybackContract } from "./playback.js";

const observation = {
  removedLegacySurface: !("AgentProvider" in api) && !("useRemoteMedia" in api),
  captureDevices: false,
  committedRenderIsolation: false,
  capturePendingOptions: false,
  captureReplacement: false,
  captureFencing: false,
  captureDisplay: false,
  captureErrors: false,
  captureSession: false,
  ownedIndependent: false,
  ownedRenewal: false,
  ownedReplacement: false,
  ownedCleanup: false,
  localPreview: false,
  capturedPreview: false,
  replaced: false,
  playbackError: false,
  playbackRetained: false,
  playbackLatestCallback: false,
  detached: false,
  audioExplicit: false,
  autoplayRespected: false,
  playbackProbeError: "",
};

declare global {
  var __pulsebeamReactObservation: Promise<typeof observation> | undefined;
}

globalThis.__pulsebeamReactObservation = (async () => {
  try {
    Object.assign(observation, await runAcquisitionContract());
    Object.assign(observation, await runConcurrencyContract());
    Object.assign(observation, await runOwnershipContract());
    Object.assign(observation, await runPlaybackContract());
    Object.assign(observation, await runAutoplayContract());
  } catch (error) {
    observation.playbackProbeError = String(error);
  }
  return observation;
})();
