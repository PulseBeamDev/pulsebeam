import {
  createContext,
  createElement,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import type * as React from "react";
import {
  attachRemoteMedia,
  createAgent,
  type Agent,
  type AgentConfig,
  type AgentSnapshot,
  type AgentState,
  type PlaybackFailure,
  type RemoteMediaAttachment,
  type RemoteMediaAttachmentOptions,
} from "@pulsebeam/web";

export { createAgent, LocalTrackCapacityError } from "@pulsebeam/web";
export { Video, Audio } from "./playback.js";
export type { AudioProps, VideoProps, PlaybackError } from "./playback.js";
export {
  useMediaDevices,
  useUserMedia,
  useDisplayMedia,
  MediaCaptureError,
} from "./capture.js";
export type {
  CaptureResult,
  DisplayMediaOptions,
  MediaCaptureErrorCode,
  MediaDevice,
  MediaDevicesResult,
  UserMediaOptions,
} from "./capture.js";
export type {
  Agent,
  AgentConfig,
  AgentEvent,
  AgentFailure,
  AgentLogging,
  AgentSnapshot,
  AgentState,
  AudioDemand,
  CatalogSnapshot,
  AudioSenderConfig,
  ConnectionState,
  FailureClass,
  FixedPlayoutDelay,
  MediaKind,
  MappingSnapshot,
  MappedRemoteAudioTrack,
  MappedRemoteVideoTrack,
  MediaTopology,
  LogLevel,
  LocalAudioTrack,
  LocalVideoTrack,
  CapturedAudioTrack,
  CapturedVideoTrack,
  PlaybackFailure,
  Participant,
  Publication,
  PublicationIntent,
  RemoteAudioSource,
  RemoteAudioTrack,
  ReceiveOptions,
  RemoteMediaAttachment,
  RemoteMediaAttachmentOptions,
  RemoteTrack,
  RemoteVideoTrack,
  SenderConfig,
  SenderEncoding,
  TopicDropReason,
  TopicMode,
  TopicPublisherStatus,
  TopicRegistration,
  TopicSnapshot,
  TopicSubscriberStatus,
  Topic,
  TrackMapping,
  TrackSelector,
  VideoDemand,
  VideoSenderConfig,
} from "@pulsebeam/web";

const AgentContext = createContext<Agent | null>(null);

export interface AgentProviderProps {
  readonly agent: Agent;
  readonly children?: React.ReactNode;
}

export function AgentProvider({
  agent,
  children,
}: AgentProviderProps): React.ReactElement {
  return createElement(AgentContext.Provider, { value: agent }, children);
}

export interface UseAgentResult extends AgentSnapshot {
  readonly connect: Agent["connect"];
  readonly disconnect: Agent["disconnect"];
  readonly localVideoTrack: Agent["localVideoTrack"];
  readonly localAudioTrack: Agent["localAudioTrack"];
  readonly setState: Agent["setState"];
  readonly replaceLocalTrack: Agent["replaceLocalTrack"];
  readonly setLocalMuted: Agent["setLocalMuted"];
  readonly reconnect: Agent["reconnect"];
  readonly sendTopic: Agent["sendTopic"];
  readonly subscribeEvents: Agent["subscribeEvents"];
}

const noSubscription = () => () => {};
const noSnapshot = () => null;

export function useAgent(config: AgentConfig): Agent | null;
export function useAgent(): UseAgentResult;
export function useAgent(config?: AgentConfig): UseAgentResult | Agent | null {
  const provided = useContext(AgentContext);
  const [owned, setOwned] = useState<{ key: string; agent: Agent } | null>(
    null,
  );
  const key =
    config === undefined
      ? null
      : JSON.stringify([config.endpoint, config.topology, config.logging]);

  useEffect(() => {
    if (!config || key === null) return;
    const agent = createAgent(config);
    setOwned({ key, agent });
    return () => agent.close();
  }, [key]);
  useEffect(() => {
    if (config && owned?.key === key)
      owned.agent.renewAuthorization(config.token);
  }, [config?.token, key, owned]);

  const agent =
    config === undefined ? provided : owned?.key === key ? owned.agent : null;
  const snapshot = useSyncExternalStore(
    agent?.subscribe ?? noSubscription,
    agent?.getSnapshot ?? noSnapshot,
    agent?.getSnapshot ?? noSnapshot,
  );
  const connect = useCallback(() => agent!.connect(), [agent]);
  const disconnect = useCallback(() => agent!.disconnect(), [agent]);
  const localVideoTrack = useCallback<Agent["localVideoTrack"]>(
    (label) => agent!.localVideoTrack(label),
    [agent],
  );
  const localAudioTrack = useCallback<Agent["localAudioTrack"]>(
    (label) => agent!.localAudioTrack(label),
    [agent],
  );
  const setState = useCallback(
    (state: AgentState): void => agent!.setState(state),
    [agent],
  );
  const replaceLocalTrack = useCallback<Agent["replaceLocalTrack"]>(
    (slot, track, sender) => agent!.replaceLocalTrack(slot, track, sender),
    [agent],
  );
  const setLocalMuted = useCallback<Agent["setLocalMuted"]>(
    (slot, muted) => agent!.setLocalMuted(slot, muted),
    [agent],
  );
  const reconnect = useCallback((): void => agent!.reconnect(), [agent]);
  const sendTopic = useCallback<Agent["sendTopic"]>(
    (name, mode, payload) => agent!.sendTopic(name, mode, payload),
    [agent],
  );
  const subscribeEvents = useCallback<Agent["subscribeEvents"]>(
    (listener) => agent!.subscribeEvents(listener),
    [agent],
  );

  const result = useMemo(
    () => ({
      ...(snapshot as AgentSnapshot),
      connect,
      disconnect,
      localVideoTrack,
      localAudioTrack,
      setState,
      replaceLocalTrack,
      setLocalMuted,
      reconnect,
      sendTopic,
      subscribeEvents,
    }),
    [
      snapshot,
      connect,
      disconnect,
      localVideoTrack,
      localAudioTrack,
      setState,
      replaceLocalTrack,
      setLocalMuted,
      reconnect,
      sendTopic,
      subscribeEvents,
    ],
  );
  if (config !== undefined) return agent;
  if (provided === null) throw new Error("useAgent requires AgentProvider");
  return result;
}

export interface UseRemoteMediaResult {
  readonly retryPlayback: () => Promise<void>;
}

export function useRemoteMedia(
  agent: Agent,
  element: React.RefObject<HTMLMediaElement | null>,
  options: RemoteMediaAttachmentOptions,
): UseRemoteMediaResult {
  const attachment = useRef<RemoteMediaAttachment | null>(null);
  const attachedAgent = useRef<Agent | null>(null);
  const attachedElement = useRef<HTMLMediaElement | null>(null);
  const onPlaybackBlocked = useRef(options.onPlaybackBlocked);
  onPlaybackBlocked.current = options.onPlaybackBlocked;

  const reportPlaybackBlocked = useCallback(
    (failure: PlaybackFailure, retry: () => Promise<void>): void =>
      onPlaybackBlocked.current?.(failure, retry),
    [],
  );

  useEffect(() => {
    const currentElement = element.current;
    const currentAttachment = attachment.current;
    if (
      currentAttachment !== null &&
      (attachedAgent.current !== agent ||
        attachedElement.current !== currentElement)
    ) {
      currentAttachment.close();
      attachment.current = null;
      attachedAgent.current = null;
      attachedElement.current = null;
    }
    if (attachment.current === null && currentElement !== null) {
      attachment.current = attachRemoteMedia(agent, currentElement, {
        publicationIds: options.publicationIds,
        onPlaybackBlocked: reportPlaybackBlocked,
      });
      attachedAgent.current = agent;
      attachedElement.current = currentElement;
    } else {
      attachment.current?.setPublicationIds(options.publicationIds);
    }
  });

  const retryPlayback = useCallback(async (): Promise<void> => {
    await attachment.current?.retryPlayback();
  }, []);

  useEffect(
    () => () => {
      attachment.current?.close();
      attachment.current = null;
      attachedAgent.current = null;
      attachedElement.current = null;
    },
    [],
  );

  return useMemo(() => ({ retryPlayback }), [retryPlayback]);
}
