import { useLayoutEffect, useRef, type ReactElement } from "react";
import { useCommittedRef } from "./committed-ref.js";
import {
  nativeCaptureTrack,
  type Agent as WebAgent,
  type CapturedVideoTrack,
  type LocalAudioTrack,
  type LocalVideoTrack,
  type SenderConfig,
  type TopicCallbacks,
} from "@pulsebeam/web";

import type { ManagedAgent } from "@pulsebeam/web/internal/react";
import { createRemoteMedia, type RemoteMedia } from "./remote.js";
import {
  videoSource,
  audioSource,
  localVideoBinding,
  publicationTrack,
  type VideoSource,
  type AudioSource,
  type PublishableVideoSource,
  type PublishableAudioSource,
} from "./sources.js";
export type {
  RemoteMedia,
  RemoteParticipant,
  RemoteVideoTrack,
  RemoteAudioTrack,
} from "./remote.js";

export type PublicationDegradation =
  | "maintain-framerate"
  | "maintain-resolution"
  | "balanced";
export interface VideoPublicationPreferences {
  readonly content?: "motion" | "detail" | "text";
  readonly degradation?: PublicationDegradation;
}
export interface AudioPublicationPreferences {
  readonly content?: "speech" | "music";
  readonly degradation?: PublicationDegradation;
}
export interface VideoPublicationProps {
  readonly source: PublishableVideoSource | null;
  readonly enabled?: boolean;
  readonly preferences?: VideoPublicationPreferences;
}
export interface AudioPublicationProps {
  readonly source: PublishableAudioSource | null;
  readonly enabled?: boolean;
  readonly preferences?: AudioPublicationPreferences;
}
export type LocalVideo = ((
  props: VideoPublicationProps,
) => ReactElement | null) &
  VideoSource & { readonly label: string };
export type LocalAudio = ((
  props: AudioPublicationProps,
) => ReactElement | null) &
  AudioSource & { readonly label: string };
export type Topic<T> = ((props: TopicCallbacks<T>) => ReactElement | null) & {
  publish(value: T): Promise<void>;
};
export interface Agent
  extends Pick<
    WebAgent,
    | "connection"
    | "error"
    | "participantId"
    | "participantExternalId"
    | "roomExternalId"
    | "authorizationExpiresAt"
    | "connect"
    | "disconnect"
    | "reconnect"
  > {
  readonly local: {
    video(label: string): LocalVideo;
    audio(label: string): LocalAudio;
  };
  readonly remote: RemoteMedia;
  topic<T>(
    name: string,
    options: { readonly mode: "reliable" | "unreliable" },
  ): Topic<T>;
}

function senderConfig(
  kind: "video" | "audio",
  preferences:
    | VideoPublicationPreferences
    | AudioPublicationPreferences
    | undefined,
): SenderConfig {
  const content =
    preferences?.content ?? (kind === "video" ? "motion" : "speech");
  const degradation = preferences?.degradation;
  if (
    !(
      kind === "video" ? ["motion", "detail", "text"] : ["speech", "music"]
    ).includes(content) ||
    (degradation !== undefined &&
      !["maintain-framerate", "maintain-resolution", "balanced"].includes(
        degradation,
      ))
  )
    throw new TypeError("invalid publication preferences");
  return {
    contentHint: content,
    ...(degradation === undefined
      ? {}
      : { degradationPreference: degradation }),
  } as SenderConfig;
}

function publication(
  raw: ManagedAgent,
  kind: "video" | "audio",
  label: string,
): LocalVideo | LocalAudio {
  let handle: LocalVideoTrack | LocalAudioTrack | null = null;
  const listeners = new Set<() => void>();
  function Publication(
    props: VideoPublicationProps | AudioPublicationProps,
  ): null {
    const owner = useRef({});
    useLayoutEffect(() => {
      handle = raw.claimPublication(owner.current, kind, label);
      const unsubscribe = handle.subscribe(() => {
        for (const listener of listeners) listener();
      });
      return () => {
        raw.releasePublication(owner.current, kind, label);
        unsubscribe();
      };
    }, []);
    const config = senderConfig(kind, props.preferences);
    const configKey = JSON.stringify(config);
    const enabled = props.enabled ?? true;
    useLayoutEffect(() => {
      const source = props.source ? publicationTrack(props.source, kind) : null;
      const track = source ? nativeCaptureTrack(source, kind) : null;
      const sync = () =>
        raw.updatePublication(
          owner.current,
          kind,
          label,
          track?.readyState === "live" ? source : null,
          enabled,
          config,
        );
      sync();
      track?.addEventListener("ended", sync);
      return () => track?.removeEventListener("ended", sync);
    }, [props.source, enabled, configKey]);
    return null;
  }
  const component = Object.assign(Publication, { kind, label }) as
    | LocalVideo
    | LocalAudio;
  if (kind === "video")
    return videoSource(
      component as LocalVideo,
      localVideoBinding(
        () =>
          handle?.enabled ? (handle.source as CapturedVideoTrack | null) : null,
        (listener) => {
          listeners.add(listener);
          return () => {
            listeners.delete(listener);
          };
        },
      ),
    );
  return audioSource(component as LocalAudio);
}

export function createReactAgent(raw: ManagedAgent): Agent {
  const videos = new Map<string, LocalVideo>();
  const audios = new Map<string, LocalAudio>();
  const topics = new Map<string, Topic<unknown>>();
  const local = <T extends LocalVideo | LocalAudio>(
    kind: "video" | "audio",
    label: string,
    handles: Map<string, T>,
  ): T => {
    if (
      typeof label !== "string" ||
      !label ||
      new TextEncoder().encode(label).length > 64
    )
      throw new TypeError("invalid local track label");
    let handle = handles.get(label);
    if (!handle) {
      handle = publication(raw, kind, label) as T;
      handles.set(label, handle);
    }
    return handle;
  };
  return Object.freeze({
    get connection() {
      return raw.connection;
    },
    get error() {
      return raw.error;
    },
    get participantId() {
      return raw.participantId;
    },
    get participantExternalId() {
      return raw.participantExternalId;
    },
    get roomExternalId() {
      return raw.roomExternalId;
    },
    get authorizationExpiresAt() {
      return raw.authorizationExpiresAt;
    },
    connect: () => raw.connect(),
    disconnect: () => raw.disconnect(),
    reconnect: () => raw.reconnect(),
    remote: createRemoteMedia(raw.remote),
    local: Object.freeze({
      video: (label: string) => local("video", label, videos),
      audio: (label: string) => local("audio", label, audios),
    }),
    topic<T>(
      name: string,
      options: { readonly mode: "reliable" | "unreliable" },
    ): Topic<T> {
      const imperative = raw.topic<T>(name, options);
      const key = `${options.mode}:${name}`;
      let component = topics.get(key) as Topic<T> | undefined;
      if (!component) {
        function Consumer(props: TopicCallbacks<T>): null {
          const callbacks = useCommittedRef(props);
          useLayoutEffect(() => {
            const owner = {};
            const release = raw.claimRelationship(owner);
            const unsubscribe = raw.listenTopic<T>(name, options, {
              onMessage: (value, metadata) =>
                callbacks.current.onMessage(value, metadata),
              onGap: (gap) => {
                if (callbacks.current.onGap) callbacks.current.onGap(gap);
                else console.warn("PulseBeam topic recovery gap", gap);
              },
              onError: (error) => {
                if (callbacks.current.onError) callbacks.current.onError(error);
                else console.error("PulseBeam topic consumer failed", error);
              },
            });
            return () => {
              unsubscribe();
              release();
            };
          }, []);
          return null;
        }
        component = Object.assign(Consumer, {
          publish: (value: T) => imperative.publish(value),
        });
        topics.set(key, component as Topic<unknown>);
      }
      return component;
    },
  });
}
