export function normalizeEndpoint(input: string): string | null {
  try {
    const url = new URL(input);
    if (!/^https?:$/.test(url.protocol) || url.search || url.hash) return null;
    let path = url.pathname.replace(/\/+$/, "");
    if (path.endsWith("/api/v1")) path = path.slice(0, -7);
    return `${url.origin}${path}`;
  } catch { return null; }
}

export const topicState = [
  { name: "chat", mode: "ordered", publish: true, subscribe: true },
  { name: "reactions", mode: "latest", publish: true, subscribe: true },
] as const;

import type { AgentState } from "@pulsebeam/react";

export function desiredState(connected: boolean, publications: readonly string[], video: AgentState["video"], playoutDelay?: { mode: "fixed"; minMs: number; maxMs: number }): AgentState {
  return { connected, publications: publications.map((slot) => ({ slot, active: true })), video, audio: { automatic: true }, topics: topicState, ...(playoutDelay ? { playoutDelay } : {}) };
}
