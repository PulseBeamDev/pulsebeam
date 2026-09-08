export type TrackKind = "audio" | "video" | "data";

export type DataMode = "reliable" | "unreliable";

export type ConnectionState =
  | "disconnected"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "failed";

export interface TrackRef {
  readonly participantId: string;
  readonly kind: TrackKind;
  readonly label: string;
}

export interface LocalAudioTrack {
  readonly kind: "audio";
  readonly label: string;
  readonly media: MediaStreamTrack;
}
export interface LocalVideoTrack {
  readonly kind: "video";
  readonly label: string;
  readonly media: MediaStreamTrack;
}
export interface LocalDataTrack {
  readonly kind: "data";
  readonly label: string;
  readonly mode: DataMode;
  readonly readable: ReadableStream<Uint8Array>;
  readonly writable: WritableStream<Uint8Array>;
}
export type LocalTrack = LocalAudioTrack | LocalVideoTrack | LocalDataTrack;

export interface RemoteAudioTrack extends TrackRef {
  readonly kind: "audio";
  readonly media: MediaStreamTrack | null;
}
export interface RemoteVideoTrack extends TrackRef {
  readonly kind: "video";
  readonly media: MediaStreamTrack | null;
}
export interface RemoteDataTrack extends TrackRef {
  readonly kind: "data";
  readonly mode: DataMode;
  readonly readable: ReadableStream<Uint8Array>;
  readonly writable: WritableStream<Uint8Array>;
}
export type RemoteTrack = RemoteAudioTrack | RemoteVideoTrack | RemoteDataTrack;

export interface ConnectionIntent {
  readonly roomId: string;
  readonly token: string;
}
export interface AgentState {
  readonly connection: ConnectionIntent | null;
  readonly publish?: readonly LocalTrack[];
  readonly subscribe?: readonly TrackRef[];
}
export interface AgentSnapshot {
  readonly connection: ConnectionState;
  readonly participantId: string | null;
  readonly tracks: readonly RemoteTrack[];
}
export interface Agent {
  setState(state: AgentState): void;
  readonly getSnapshot: () => AgentSnapshot;
  readonly subscribe: (listener: () => void) => () => void;
  close(): void;
}
