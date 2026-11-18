import type { AgentSnapshot, MediaKind, TopicMode } from "./types.js";

export interface Participant {
  readonly participantId: string;
}

export interface Publication {
  readonly id: string;
  readonly participantId: string;
  readonly kind: MediaKind;
  readonly label: string;
}

export type RemoteTrack = Readonly<{
  publicationId: string;
  participantId: string;
  kind: MediaKind;
  media: MediaStreamTrack;
}>;

export type CatalogTrack = Readonly<{
  id: string;
  participantId: string;
  label: string;
}> &
  (
    | Readonly<{ kind: MediaKind }>
    | Readonly<{ kind: "data"; delivery: TopicMode }>
  );

export interface CatalogSnapshot {
  readonly revision: number;
  readonly participants: readonly Participant[];
  readonly publications: readonly Publication[];
  readonly tracks: readonly CatalogTrack[];
}

export interface TrackMapping {
  readonly receiverIndex: number;
  readonly publicationId: string;
}

export interface MappingSnapshot {
  readonly acceptedIntentRevision: number;
  readonly video: readonly TrackMapping[];
  readonly audio: readonly TrackMapping[];
}

export interface ReconciliationSnapshot extends AgentSnapshot {
  readonly catalog: CatalogSnapshot;
  readonly mapping: MappingSnapshot;
  readonly tracks: Readonly<Record<string, RemoteTrack>>;
}

export function publicSnapshot({
  catalog: _catalog,
  mapping: _mapping,
  tracks: _tracks,
  ...snapshot
}: ReconciliationSnapshot): AgentSnapshot {
  return Object.freeze(snapshot);
}
