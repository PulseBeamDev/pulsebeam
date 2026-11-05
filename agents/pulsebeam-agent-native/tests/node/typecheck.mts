import { LogLevel, TopicMode, type AgentConfig } from "@pulsebeam/agent/core";
import { Agent, InputFrame, type AgentLike, type NativeAgentConfig } from "@pulsebeam/agent/native";

declare const config: AgentConfig;
declare const nativeConfig: NativeAgentConfig;
config.logLevel = LogLevel.Warn;
const agent: Promise<AgentLike> = Agent.create(config);
const nativeAgent: Promise<AgentLike> = Agent.createWithMedia(nativeConfig);
async function binaryInputs(agent: AgentLike) {
  const camera = await agent.createVideoInput({
    encodings: [{ rid: undefined, scaleDownBy: 1, maxBitrateBps: undefined, maxFramerate: undefined, temporalLayers: 1 }],
    dependencyDescriptor: true,
  });
  const data = new Uint8Array(160 * 96 + 2 * 80 * 48);
  await camera.submit(new InputFrame.I420({
    frame: { width: 160, height: 96, timestampUs: await camera.captureTimeUs(), data, rotationDegrees: 0 },
  }));
  await agent.data("foreign-exchange", TopicMode.Ordered).send(data);
}
void [agent, nativeAgent, binaryInputs];
