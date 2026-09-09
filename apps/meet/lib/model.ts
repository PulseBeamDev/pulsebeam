export function normalizeEndpoint(input: string): string | null {
  try {
    const url = new URL(input);
    if (!/^https?:$/.test(url.protocol) || url.search || url.hash) return null;
    let path = url.pathname.replace(/\/+$/, "");
    if (path.endsWith("/api/v1")) path = path.slice(0, -7);
    return `${url.origin}${path}`;
  } catch {
    return null;
  }
}

export const topicState = [
  { name: "chat", mode: "ordered", publish: true, subscribe: true },
  { name: "reactions", mode: "latest", publish: true, subscribe: true },
] as const;

import type { AgentState } from "@pulsebeam/react";

export function desiredState(
  connected: boolean,
  publications: readonly string[],
  video: AgentState["video"],
  playoutDelay?: { mode: "fixed"; minMs: number; maxMs: number },
): AgentState {
  return {
    connected,
    publications: publications.map((slot) => ({ slot, active: true })),
    video,
    audio: { automatic: true },
    topics: topicState,
    ...(playoutDelay ? { playoutDelay } : {}),
  };
}

export const videoHeights = [0, 90, 180, 360, 540, 720, 1080] as const;

export function quantizeHeight(height: number): number {
  return videoHeights.find((candidate) => candidate >= height) ?? 1080;
}

export interface VideoSelection {
  readonly id: string;
  readonly slot: number;
  readonly height: number;
  readonly priority: number;
}

/** Keep slots stable for retained publications while choosing deterministic new ones. */
export function allocateVideo(
  publicationIds: readonly string[],
  pinned: string | null,
  previous: readonly VideoSelection[],
  heights: Readonly<Record<string, number>>,
  spotlightHeight: number,
): VideoSelection[] {
  const eligible = [...new Set(publicationIds)].sort();
  const spotlight =
    pinned && eligible.includes(pinned) ? pinned : (eligible[0] ?? null);
  const ranked = eligible
    .map((id) => ({
      id,
      priority: id === spotlight ? 200 : 10,
      height: quantizeHeight(
        Math.max(
          id === spotlight ? 360 : 90,
          id === spotlight ? spotlightHeight : (heights[id] ?? 0),
        ),
      ),
    }))
    .sort(
      (a, b) =>
        b.priority - a.priority ||
        b.height - a.height ||
        a.id.localeCompare(b.id),
    )
    .slice(0, 7);
  const retained = new Map(
    previous
      .filter((entry) => ranked.some((candidate) => candidate.id === entry.id))
      .map((entry) => [entry.id, entry.slot]),
  );
  const free = Array.from({ length: 7 }, (_, slot) => slot).filter(
    (slot) => ![...retained.values()].includes(slot),
  );
  return ranked.map((entry) => ({
    ...entry,
    slot: retained.get(entry.id) ?? free.shift()!,
  }));
}
