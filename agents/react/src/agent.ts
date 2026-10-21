import type {
  Agent as WebAgent,
  AudioTrackSpec,
  DataEvent,
  DataEventsOptions,
  DataTrackSpec,
  Delivery,
  LocalAudioTrack,
  LocalDataTrack as WebLocalDataTrack,
  LocalVideoTrack,
  RemoteAudioTrack as WebRemoteAudioTrack,
  RemoteDataTrack as WebRemoteDataTrack,
  RemoteParticipant as WebRemoteParticipant,
  RemoteVideoTrack as WebRemoteVideoTrack,
  VideoTrackSpec,
} from "@pulsebeam/web";
import type { ManagedAgent } from "@pulsebeam/web/internal/react";

export interface LocalDataTrack<D extends Delivery = Delivery> {
  readonly kind: "data";
  readonly label: string;
  readonly delivery: D;
  send(data: Uint8Array): Promise<void>;
}

export interface RemoteVideoTrack {
  readonly kind: "video";
  readonly id: string | null;
  readonly participantId: string;
  readonly label: string;
  readonly active: boolean;
  subscribe(listener: () => void): () => void;
}

export interface RemoteAudioTrack {
  readonly kind: "audio";
  readonly id: string | null;
  readonly participantId: string;
  readonly label: string;
  readonly active: boolean;
  subscribe(listener: () => void): () => void;
}

export interface RemoteDataTrack<D extends Delivery = Delivery> {
  readonly kind: "data";
  readonly id: string | null;
  readonly participantId: string;
  readonly label: string;
  readonly delivery: D;
  readonly active: boolean;
  subscribe(listener: () => void): () => void;
}

export type RemoteTrack = RemoteVideoTrack | RemoteAudioTrack | RemoteDataTrack;

export interface RemoteParticipant {
  readonly participantId: string;
  readonly active: boolean;
  readonly tracks: readonly RemoteTrack[];
  track(spec: VideoTrackSpec): RemoteVideoTrack;
  track(spec: AudioTrackSpec): RemoteAudioTrack;
  track<D extends Delivery>(spec: DataTrackSpec<D>): RemoteDataTrack<D>;
}

export interface RemoteMedia {
  readonly participants: readonly RemoteParticipant[];
  participant(participantId: string): RemoteParticipant;
}

export interface Agent
  extends Pick<
    WebAgent,
    | "connection"
    | "error"
    | "participantId"
    | "roomId"
    | "authorizationExpiresAt"
    | "connect"
    | "disconnect"
  > {
  readonly remote: RemoteMedia;
  track(spec: VideoTrackSpec): LocalVideoTrack;
  track(spec: AudioTrackSpec): LocalAudioTrack;
  track<D extends Delivery>(spec: DataTrackSpec<D>): LocalDataTrack<D>;
}

const managedAgents = new WeakMap<Agent, ManagedAgent>();
const webDataTracks = new WeakMap<
  LocalDataTrack | RemoteDataTrack,
  WebLocalDataTrack | WebRemoteDataTrack
>();
const webVideos = new WeakMap<RemoteVideoTrack, WebRemoteVideoTrack>();

export function managedAgent(agent: Agent): ManagedAgent {
  const managed = managedAgents.get(agent);
  if (!managed) throw new TypeError("invalid Agent");
  return managed;
}
export function dataEvents<D extends Delivery>(
  value: LocalDataTrack<D> | RemoteDataTrack<D>,
  options?: DataEventsOptions,
): AsyncIterableIterator<DataEvent<D>> {
  const raw = webDataTracks.get(value);
  if (!raw) throw new TypeError("invalid data track");
  return raw.events(options) as AsyncIterableIterator<DataEvent<D>>;
}
export function remoteVideo(value: RemoteVideoTrack): WebRemoteVideoTrack {
  const raw = webVideos.get(value);
  if (!raw) throw new TypeError("invalid remote video track");
  return raw;
}

export function createReactAgent(raw: ManagedAgent): Agent {
  const data = new WeakMap<object, LocalDataTrack | RemoteDataTrack>();
  const videos = new WeakMap<WebRemoteVideoTrack, RemoteVideoTrack>();
  const audios = new WeakMap<WebRemoteAudioTrack, RemoteAudioTrack>();
  const participants = new WeakMap<WebRemoteParticipant, RemoteParticipant>();
  const collections = new WeakMap<object, readonly unknown[]>();
  const collection = <A extends object, B>(
    values: readonly A[],
    wrap: (value: A) => B,
  ): readonly B[] => {
    const existing = collections.get(values as object) as
      | readonly B[]
      | undefined;
    if (existing) return existing;
    const next = Object.freeze(values.map(wrap));
    collections.set(values as object, next);
    return next;
  };

  const wrapData = <D extends Delivery>(
    value: WebLocalDataTrack<D> | WebRemoteDataTrack<D>,
  ): LocalDataTrack<D> | RemoteDataTrack<D> => {
    let wrapped = data.get(value) as
      | LocalDataTrack<D>
      | RemoteDataTrack<D>
      | undefined;
    if (wrapped) return wrapped;
    wrapped =
      "participantId" in value
        ? Object.freeze({
            kind: value.kind,
            label: value.label,
            delivery: value.delivery,
            participantId: value.participantId,
            get id() {
              return value.id;
            },
            get active() {
              return value.active;
            },
            subscribe: (listener: () => void) => value.subscribe(listener),
          })
        : Object.freeze({
            kind: value.kind,
            label: value.label,
            delivery: value.delivery,
            send: (bytes: Uint8Array) => value.send(bytes),
          });
    data.set(value, wrapped);
    webDataTracks.set(wrapped, value);
    return wrapped;
  };
  const wrapVideo = (value: WebRemoteVideoTrack): RemoteVideoTrack => {
    let wrapped = videos.get(value);
    if (!wrapped) {
      wrapped = Object.freeze({
        kind: value.kind,
        label: value.label,
        participantId: value.participantId,
        get id() {
          return value.id;
        },
        get active() {
          return value.active;
        },
        subscribe: (listener: () => void) => value.subscribe(listener),
      });
      videos.set(value, wrapped);
      webVideos.set(wrapped, value);
    }
    return wrapped;
  };
  const wrapAudio = (value: WebRemoteAudioTrack): RemoteAudioTrack => {
    let wrapped = audios.get(value);
    if (!wrapped) {
      wrapped = Object.freeze({
        kind: value.kind,
        label: value.label,
        participantId: value.participantId,
        get id() {
          return value.id;
        },
        get active() {
          return value.active;
        },
        subscribe: (listener: () => void) => value.subscribe(listener),
      });
      audios.set(value, wrapped);
    }
    return wrapped;
  };
  const wrapParticipant = (value: WebRemoteParticipant): RemoteParticipant => {
    let wrapped = participants.get(value);
    if (wrapped) return wrapped;
    const track = (spec: VideoTrackSpec | AudioTrackSpec | DataTrackSpec) => {
      if (spec.kind === "video") return wrapVideo(value.track(spec));
      if (spec.kind === "audio") return wrapAudio(value.track(spec));
      return wrapData(value.track(spec)) as RemoteDataTrack;
    };
    wrapped = Object.freeze({
      participantId: value.participantId,
      get active() {
        return value.active;
      },
      get tracks() {
        return collection(value.tracks, (entry) =>
          entry.kind === "video"
            ? wrapVideo(entry)
            : entry.kind === "audio"
              ? wrapAudio(entry)
              : (wrapData(entry) as RemoteDataTrack),
        );
      },
      track: track as RemoteParticipant["track"],
    });
    participants.set(value, wrapped);
    return wrapped;
  };
  const remote: RemoteMedia = Object.freeze({
    get participants() {
      return collection(raw.remote.participants, wrapParticipant);
    },
    participant: (participantId: string) =>
      wrapParticipant(raw.remote.participant(participantId)),
  });
  const facade: Agent = Object.freeze({
    get connection() {
      return raw.connection;
    },
    get error() {
      return raw.error;
    },
    get participantId() {
      return raw.participantId;
    },
    get roomId() {
      return raw.roomId;
    },
    get authorizationExpiresAt() {
      return raw.authorizationExpiresAt;
    },
    get remote() {
      return remote;
    },
    connect: () => raw.connect(),
    disconnect: () => raw.disconnect(),
    track: ((spec: VideoTrackSpec | AudioTrackSpec | DataTrackSpec) => {
      if (spec.kind === "video") return raw.track(spec);
      if (spec.kind === "audio") return raw.track(spec);
      return wrapData(raw.track(spec)) as LocalDataTrack;
    }) as Agent["track"],
  });
  managedAgents.set(facade, raw);
  return facade;
}
