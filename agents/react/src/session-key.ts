import type { AgentConfig } from "@pulsebeam/web";

export function sessionKey(config: AgentConfig): string {
  return JSON.stringify([
    config.endpoint,
    config.topology?.localVideos ?? 2,
    config.topology?.localAudios ?? 2,
    config.topology?.remoteVideos ?? 16,
    config.topology?.remoteAudios ?? 8,
  ]);
}
