import {
  createContext,
  createElement,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useSyncExternalStore,
} from "react";
import type * as React from "react";
import {
  attachRemoteMedia,
  type Agent,
  type AgentSnapshot,
  type AgentState,
  type PlaybackFailure,
  type RemoteMediaAttachment,
  type RemoteMediaAttachmentOptions,
} from "@pulsebeam/web";

export { createAgent } from "@pulsebeam/web";
export type {
  Agent,
  AgentConfig,
  AgentEvent,
  AgentFailure,
  AgentLogging,
  AgentSnapshot,
  AgentState,
  AudioBinding,
  AudioDemand,
  AudioSenderConfig,
  ConnectionState,
  FailureClass,
  FixedPlayoutDelay,
  MediaKind,
  MediaTopology,
  LogLevel,
  PlaybackFailure,
  Participant,
  Publication,
  PublicationIntent,
  RemoteAudioTrack,
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
  VideoBinding,
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
  readonly setState: Agent["setState"];
  readonly replaceLocalTrack: Agent["replaceLocalTrack"];
  readonly setLocalMuted: Agent["setLocalMuted"];
  readonly reconnect: Agent["reconnect"];
  readonly sendTopic: Agent["sendTopic"];
  readonly subscribeEvents: Agent["subscribeEvents"];
}

export function useAgent(): UseAgentResult {
  const agent = useContext(AgentContext);
  if (agent === null) {
    throw new Error("useAgent requires AgentProvider");
  }

  const snapshot = useSyncExternalStore(
    agent.subscribe,
    agent.getSnapshot,
    agent.getSnapshot,
  );
  const setState = useCallback(
    (state: AgentState): void => agent.setState(state),
    [agent],
  );
  const replaceLocalTrack = useCallback<Agent["replaceLocalTrack"]>(
    (slot, track, config) => agent.replaceLocalTrack(slot, track, config),
    [agent],
  );
  const setLocalMuted = useCallback<Agent["setLocalMuted"]>(
    (slot, muted) => agent.setLocalMuted(slot, muted),
    [agent],
  );
  const reconnect = useCallback((): void => agent.reconnect(), [agent]);
  const sendTopic = useCallback<Agent["sendTopic"]>(
    (name, mode, payload) => agent.sendTopic(name, mode, payload),
    [agent],
  );
  const subscribeEvents = useCallback<Agent["subscribeEvents"]>(
    (listener) => agent.subscribeEvents(listener),
    [agent],
  );

  return useMemo(
    () => ({
      ...snapshot,
      setState,
      replaceLocalTrack,
      setLocalMuted,
      reconnect,
      sendTopic,
      subscribeEvents,
    }),
    [
      snapshot,
      setState,
      replaceLocalTrack,
      setLocalMuted,
      reconnect,
      sendTopic,
      subscribeEvents,
    ],
  );
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
