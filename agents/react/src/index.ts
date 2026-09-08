import {
  createContext,
  createElement,
  useCallback,
  useContext,
  useMemo,
  useSyncExternalStore,
} from "react";
import type * as React from "react";
import type { Agent, AgentSnapshot, AgentState } from "@pulsebeam/web";

export { createAgent } from "@pulsebeam/web";
export type {
  Agent,
  AgentConfig,
  AgentEvent,
  AgentFailure,
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
  Participant,
  Publication,
  PublicationIntent,
  RemoteAudioTrack,
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
