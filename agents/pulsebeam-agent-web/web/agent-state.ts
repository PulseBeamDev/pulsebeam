import type {
  AgentConfig,
  AgentSnapshot,
  AgentState,
  ConnectionState,
  LogLevel,
  MediaTopology,
  RemoteTrack,
  TopicSnapshot,
  TopicPublisherStatus,
  TopicSubscriberStatus,
} from "./types.js";

export interface RuntimeConfig {
  readonly endpoint: string;
  readonly token: string;
  readonly topology: Required<MediaTopology>;
  readonly logLevel: LogLevel;
}

const EMPTY_ARRAY: readonly never[] = Object.freeze([]);
const EMPTY_TRACKS: Readonly<Record<string, RemoteTrack>> = Object.freeze({});

export function emptyTopics(): TopicSnapshot {
  return Object.freeze({
    publishers: EMPTY_ARRAY,
    subscribers: EMPTY_ARRAY,
    acceptedSends: 0,
    droppedSends: 0,
    deliveredMessages: 0,
    resynchronizations: 0,
    channelFailures: 0,
  });
}

export function emptySnapshot(connection: ConnectionState): AgentSnapshot {
  return Object.freeze({
    version: 0,
    desiredRevision: 0,
    connection,
    generation: null,
    participantId: null,
    participantExternalId: null,
    roomExternalId: null,
    authorizationExpiresAt: null,
    catalog: Object.freeze({
      revision: 0,
      participants: EMPTY_ARRAY,
      publications: EMPTY_ARRAY,
    }),
    mapping: Object.freeze({
      acceptedIntentRevision: 0,
      video: EMPTY_ARRAY,
      audio: EMPTY_ARRAY,
    }),
    tracks: EMPTY_TRACKS,
    topics: emptyTopics(),
    failure: null,
  });
}

export function copyConfig(config: AgentConfig): RuntimeConfig {
  return Object.freeze({
    endpoint: config.endpoint,
    token: config.token,
    topology: Object.freeze({
      localVideos: config.topology?.localVideos ?? 2,
      localAudios: config.topology?.localAudios ?? 2,
      remoteVideos: config.topology?.remoteVideos ?? 16,
      remoteAudios: config.topology?.remoteAudios ?? 8,
    }),
    logLevel: config.logging?.level ?? "warn",
  });
}

export function validatePlayoutDelay(state: AgentState): void {
  const delays = [
    ...(state.video ?? []).map((demand) => demand.playoutDelay),
    ...(state.audio?.playoutDelays ?? []).map((entry) => entry.playoutDelay),
    ...(state.audio?.selectorDelays ?? []).map((entry) => entry.playoutDelay),
  ];
  for (const delay of delays) {
    if (delay === undefined) continue;
    if (
      delay.mode !== "fixed" ||
      !Number.isInteger(delay.minMs) ||
      !Number.isInteger(delay.maxMs) ||
      delay.minMs < 0 ||
      delay.maxMs < 0 ||
      delay.minMs > 0xffffffff ||
      delay.maxMs > 0xffffffff
    ) {
      throw new RangeError(
        "playout delay requires unsigned 32-bit millisecond bounds",
      );
    }
  }
}

export function copyState(state: AgentState): AgentState {
  return Object.freeze({
    connected: state.connected,
    publications: Object.freeze(
      (state.publications ?? []).map((publication) =>
        Object.freeze({ ...publication }),
      ),
    ),
    video: Object.freeze(
      (state.video ?? []).map((demand) => {
        const fields = {
          slot: demand.slot,
          height: demand.height,
          minHeight: demand.minHeight,
          minFps: demand.minFps,
          priority: demand.priority,
          ...(demand.playoutDelay
            ? { playoutDelay: Object.freeze({ ...demand.playoutDelay }) }
            : {}),
        };
        return demand.selector
          ? Object.freeze({
              ...fields,
              selector: Object.freeze({ ...demand.selector }),
            })
          : Object.freeze({ ...fields, trackId: demand.trackId! });
      }),
    ),
    audio: Object.freeze({
      pinned: Object.freeze([...(state.audio?.pinned ?? [])]),
      selected: Object.freeze(
        (state.audio?.selected ?? []).map((selector) =>
          Object.freeze({ ...selector }),
        ),
      ),
      automatic: state.audio?.automatic ?? true,
      playoutDelays: Object.freeze(
        (state.audio?.playoutDelays ?? []).map((entry) =>
          Object.freeze({
            trackId: entry.trackId,
            playoutDelay: Object.freeze({ ...entry.playoutDelay }),
          }),
        ),
      ),
      selectorDelays: Object.freeze(
        (state.audio?.selectorDelays ?? []).map((entry) =>
          Object.freeze({
            selector: Object.freeze({ ...entry.selector }),
            playoutDelay: Object.freeze({ ...entry.playoutDelay }),
          }),
        ),
      ),
    }),
    topics: Object.freeze(
      (state.topics ?? []).map((topic) => Object.freeze({ ...topic })),
    ),
  });
}

export function desiredValue(state: AgentState): object {
  return {
    connected: state.connected,
    publications: state.publications,
    video: state.video,
    audio: state.audio,
    topics: state.topics,
  };
}

export function freezeTopicSnapshot(topics: TopicSnapshot): TopicSnapshot {
  return Object.freeze({
    publishers: Object.freeze(
      topics.publishers.map((publisher) => Object.freeze({ ...publisher })),
    ) as readonly TopicPublisherStatus[],
    subscribers: Object.freeze(
      topics.subscribers.map((subscriber) => Object.freeze({ ...subscriber })),
    ) as readonly TopicSubscriberStatus[],
    acceptedSends: topics.acceptedSends,
    droppedSends: topics.droppedSends,
    deliveredMessages: topics.deliveredMessages,
    resynchronizations: topics.resynchronizations,
    channelFailures: topics.channelFailures,
  });
}
