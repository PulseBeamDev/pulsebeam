import {
  AgentProvider,
  useAgent,
  type AgentProviderProps,
  type UseAgentResult,
} from "@pulsebeam/react";
import type { Agent, AgentSnapshot, AgentState } from "@pulsebeam/web";
import type { ReactElement, ReactNode } from "react";

declare const agent: Agent;
declare const children: ReactNode;
declare const snapshot: AgentSnapshot;
declare const state: AgentState;

const props: AgentProviderProps = { agent, children };
const provider: ReactElement = AgentProvider(props);
const result: UseAgentResult = useAgent();

const connection: AgentSnapshot["connection"] = result.connection;
const participantId: AgentSnapshot["participantId"] = result.participantId;
const tracks: AgentSnapshot["tracks"] = result.tracks;
result.setState(state);

void provider;
void snapshot;
void connection;
void participantId;
void tracks;
