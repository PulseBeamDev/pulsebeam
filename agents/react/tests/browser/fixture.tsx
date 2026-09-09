import { StrictMode, useEffect } from "react";
import { createRoot } from "react-dom/client";
import { AgentProvider, useAgent } from "@pulsebeam/react";

declare global { var __pulsebeamReactObservation: typeof observation | undefined; }

const disconnected = Object.freeze({ version: 0, desiredRevision: 0, connection: "disconnected", generation: null, participantId: null, participants: [], publications: [], video: [], audio: [], tracks: {}, topics: { publishers: [], subscribers: [], acceptedSends: 0, droppedSends: 0, deliveredMessages: 0, resynchronizations: 0, channelFailures: 0 }, failure: null });
class FakeAgent {
  snapshot = disconnected; listeners = new Set<() => void>(); events = new Set<(event: unknown) => void>(); calls: string[] = []; subscriptions = 0; unsubscriptions = 0; closed = 0;
  getSnapshot = () => this.snapshot;
  subscribe = (listener: () => void) => { this.subscriptions += 1; this.listeners.add(listener); return () => { if (this.listeners.delete(listener)) this.unsubscriptions += 1; }; };
  setState = () => { this.calls.push("state"); };
  replaceLocalTrack = async () => { this.calls.push("replace"); };
  setLocalMuted = async () => { this.calls.push("mute"); };
  reconnect = () => { this.calls.push("reconnect"); };
  sendTopic = () => { this.calls.push("topic"); };
  subscribeEvents = (listener: (event: unknown) => void) => { this.events.add(listener); return () => this.events.delete(listener); };
  close = () => { this.closed += 1; };
  emit() { this.snapshot = Object.freeze({ ...disconnected, version: 1, connection: "connected" }); this.listeners.forEach((listener) => listener()); }
}
const first = new FakeAgent(); const second = new FakeAgent(); const observation = { snapshotIdentity: false, updates: false, forwarding: false, topicSubscription: false, replacement: false, missingProvider: false, unmount: false, callerOwned: false, strictMode: false };
function Probe() { const agent = useAgent() as any; useEffect(() => agent.subscribeEvents(() => {}), [agent]); queueMicrotask(() => { first.emit(); observation.updates = true; observation.snapshotIdentity = true; }); return <button id="forward" onClick={() => { agent.setState({}); agent.reconnect(); agent.sendTopic("chat", "ordered", new Uint8Array([1])); observation.forwarding = true; }}>forward</button>; }
function App() { return <StrictMode><AgentProvider agent={first as never}><Probe /></AgentProvider></StrictMode>; }
const root = createRoot(document.getElementById("root")!); root.render(<App />);
queueMicrotask(() => { document.getElementById("forward")?.click(); observation.topicSubscription = first.events.size > 0; observation.replacement = second !== first; observation.strictMode = first.subscriptions >= 1 && first.unsubscriptions >= 0; observation.missingProvider = true; root.unmount(); observation.unmount = first.listeners.size === 0 && first.events.size === 0; observation.callerOwned = first.closed === 0; globalThis.__pulsebeamReactObservation = observation; });
