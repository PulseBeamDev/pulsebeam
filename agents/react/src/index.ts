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

export interface UseAgentResult {
  readonly connection: AgentSnapshot["connection"];
  readonly participantId: AgentSnapshot["participantId"];
  readonly tracks: AgentSnapshot["tracks"];
  setState(state: AgentState): void;
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

  return useMemo(
    () => ({
      connection: snapshot.connection,
      participantId: snapshot.participantId,
      tracks: snapshot.tracks,
      setState,
    }),
    [snapshot, setState],
  );
}
