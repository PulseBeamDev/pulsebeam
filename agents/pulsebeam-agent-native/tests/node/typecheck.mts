import { LogLevel, type AgentConfig } from "@pulsebeam/agent/core";
import { Agent, type AgentLike, type NativeAgentConfig } from "@pulsebeam/agent/native";

declare const config: NativeAgentConfig;
const session: AgentConfig = config.session;
session.logLevel = LogLevel.Warn;
const agent: Promise<AgentLike> = Agent.create(config);
void agent;
