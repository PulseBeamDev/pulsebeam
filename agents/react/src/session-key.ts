import type { AgentConfig } from "@pulsebeam/web";

export function sessionKey(config: AgentConfig): string {
  return JSON.stringify([config.endpoint]);
}
