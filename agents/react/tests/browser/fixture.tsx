import { Component, StrictMode, useEffect, useRef, useState } from "react";
import type { ErrorInfo, ReactNode } from "react";
import { createRoot } from "react-dom/client";
import { AgentProvider, useAgent, useRemoteMedia } from "@pulsebeam/react";
import type { AgentSnapshot } from "@pulsebeam/react";

const disconnected: AgentSnapshot = Object.freeze({
  version: 0,
  desiredRevision: 0,
  connection: "disconnected",
  generation: null,
  participantId: null,
  participants: [],
  publications: [],
  video: [],
  audio: [],
  tracks: {},
  topics: {
    publishers: [],
    subscribers: [],
    acceptedSends: 0,
    droppedSends: 0,
    deliveredMessages: 0,
    resynchronizations: 0,
    channelFailures: 0,
  },
  failure: null,
});

type Call = { method: string; arguments: unknown[] };

class FakeAgent {
  snapshot = disconnected;
  listeners = new Set<() => void>();
  events = new Set<(event: unknown) => void>();
  calls: Call[] = [];
  subscriptions = 0;
  unsubscriptions = 0;
  eventSubscriptions = 0;
  eventUnsubscriptions = 0;
  closed = 0;

  getSnapshot = () => this.snapshot;

  subscribe = (listener: () => void) => {
    this.subscriptions += 1;
    this.listeners.add(listener);
    return () => {
      if (this.listeners.delete(listener)) this.unsubscriptions += 1;
    };
  };

  setState = (...arguments_: unknown[]) => {
    this.calls.push({ method: "setState", arguments: arguments_ });
  };

  replaceLocalTrack = async (...arguments_: unknown[]) => {
    this.calls.push({ method: "replaceLocalTrack", arguments: arguments_ });
  };

  setLocalMuted = async (...arguments_: unknown[]) => {
    this.calls.push({ method: "setLocalMuted", arguments: arguments_ });
  };

  reconnect = (...arguments_: unknown[]) => {
    this.calls.push({ method: "reconnect", arguments: arguments_ });
  };

  sendTopic = (...arguments_: unknown[]) => {
    this.calls.push({ method: "sendTopic", arguments: arguments_ });
  };

  subscribeEvents = (listener: (event: unknown) => void) => {
    this.eventSubscriptions += 1;
    this.events.add(listener);
    return () => {
      if (this.events.delete(listener)) this.eventUnsubscriptions += 1;
    };
  };

  close = () => {
    this.closed += 1;
  };

  emitSame() {
    this.listeners.forEach((listener) => listener());
  }

  emitNext(version: number) {
    this.snapshot = Object.freeze({
      ...disconnected,
      version,
      connection: "connected",
    });
    this.listeners.forEach((listener) => listener());
  }

  emitEvent(event: unknown) {
    this.events.forEach((listener) => listener(event));
  }

  emitTracks(tracks: AgentSnapshot["tracks"]) {
    this.snapshot = Object.freeze({ ...this.snapshot, tracks });
    this.listeners.forEach((listener) => listener());
  }
}

const first = new FakeAgent();
const second = new FakeAgent();
const observation = {
  snapshotIdentity: false,
  updates: false,
  forwarding: false,
  topicSubscription: false,
  replacement: false,
  missingProvider: false,
  unmount: false,
  callerOwned: false,
  strictMode: false,
  playbackRetained: false,
  playbackSelection: false,
  playbackLatestCallback: false,
  playbackReplacement: false,
  playbackUnmount: false,
  playbackStrictMode: false,
};

declare global {
  var __pulsebeamReactObservation: typeof observation | undefined;
}

let renders = 0;
let current: ReturnType<typeof useAgent> | undefined;
let receivedEvent: unknown;

function Probe() {
  const agent = useAgent();
  renders += 1;
  current = agent;
  useEffect(
    () => agent.subscribeEvents((event) => (receivedEvent = event)),
    [agent.subscribeEvents],
  );
  return (
    <button
      id="forward"
      onClick={() => {
        const state = { connected: true, publications: [], video: [] };
        const track = { id: "sentinel-track" } as MediaStreamTrack;
        agent.setState(state);
        void agent.replaceLocalTrack("camera", track, {
          contentHint: "motion",
        });
        void agent.setLocalMuted("camera", true);
        agent.reconnect();
        agent.sendTopic("chat", "ordered", new Uint8Array([1, 2, 3]));
      }}
    >
      forward
    </button>
  );
}

function App() {
  const [agent, setAgent] = useState(first);
  return (
    <StrictMode>
      <AgentProvider agent={agent as never}>
        <Probe />
      </AgentProvider>
      <button id="replace" onClick={() => setAgent(second)}>
        replace
      </button>
    </StrictMode>
  );
}

let missingProviderMessage: string | undefined;

class ErrorBoundary extends Component<
  { children: ReactNode },
  { failed: boolean }
> {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  componentDidCatch(error: Error, _info: ErrorInfo) {
    missingProviderMessage = error.message;
  }

  render() {
    return this.state.failed ? (
      <span id="missing-provider" />
    ) : (
      this.props.children
    );
  }
}

function MissingProviderProbe() {
  useAgent();
  return null;
}

const playbackFirst = new FakeAgent();
const playbackSecond = new FakeAgent();
let playbackVideo: HTMLVideoElement | null = null;
let playbackRetry: (() => Promise<void>) | undefined;
let blockedCallbackVersion = 0;
let rejectPlayback = false;
let playAttempts = 0;

function PlaybackProbe() {
  const [agent, setAgent] = useState(playbackFirst);
  const [publicationIds, setPublicationIds] = useState(["first"]);
  const [callbackVersion, setCallbackVersion] = useState(1);
  const [elementVersion, setElementVersion] = useState(1);
  const element = useRef<HTMLVideoElement>(null);
  const { retryPlayback } = useRemoteMedia(agent as never, element, {
    publicationIds,
    onPlaybackBlocked: () => {
      blockedCallbackVersion = callbackVersion;
    },
  });
  useEffect(() => {
    playbackVideo = element.current;
    playbackRetry = retryPlayback;
  });

  return (
    <>
      <video key={elementVersion} ref={element} />
      <button
        id="playback-select"
        onClick={() => setPublicationIds(["second"])}
      >
        select
      </button>
      <button id="playback-callback" onClick={() => setCallbackVersion(2)}>
        callback
      </button>
      <button id="playback-element" onClick={() => setElementVersion(2)}>
        element
      </button>
      <button id="playback-agent" onClick={() => setAgent(playbackSecond)}>
        agent
      </button>
    </>
  );
}

const waitFor = async (condition: () => boolean) => {
  const deadline = Date.now() + 2_000;
  while (!condition()) {
    if (Date.now() >= deadline)
      throw new Error("React contract step timed out");
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
};

const root = createRoot(document.getElementById("root")!);
root.render(<App />);
const playbackHost = document.createElement("div");
document.body.append(playbackHost);
const playbackRoot = createRoot(playbackHost);
const originalPlay = HTMLMediaElement.prototype.play;
HTMLMediaElement.prototype.play = function () {
  playAttempts += 1;
  return rejectPlayback
    ? Promise.reject(new Error("blocked playback"))
    : Promise.resolve();
};
playbackRoot.render(
  <StrictMode>
    <PlaybackProbe />
  </StrictMode>,
);

void (async () => {
  await waitFor(
    () =>
      first.listeners.size === 1 &&
      first.events.size === 1 &&
      current !== undefined,
  );

  const snapshotBeforeSame = current;
  const rendersBeforeSameSnapshot = renders;
  first.emitSame();
  await new Promise((resolve) => setTimeout(resolve, 20));
  observation.snapshotIdentity =
    Object.is(current, snapshotBeforeSame) &&
    renders === rendersBeforeSameSnapshot;

  first.emitNext(1);
  await waitFor(() => current?.version === 1);
  observation.updates = current?.connection === "connected";

  document.getElementById("forward")!.click();
  await waitFor(() => first.calls.length === 5);
  const [setState, replace, mute, reconnect, topic] = first.calls;
  observation.forwarding =
    setState.method === "setState" &&
    (setState.arguments[0] as { connected?: boolean }).connected === true &&
    replace.method === "replaceLocalTrack" &&
    replace.arguments[0] === "camera" &&
    (replace.arguments[1] as { id?: string }).id === "sentinel-track" &&
    (replace.arguments[2] as { contentHint?: string }).contentHint ===
      "motion" &&
    mute.method === "setLocalMuted" &&
    mute.arguments[0] === "camera" &&
    mute.arguments[1] === true &&
    reconnect.method === "reconnect" &&
    topic.method === "sendTopic" &&
    topic.arguments[0] === "chat" &&
    topic.arguments[1] === "ordered" &&
    topic.arguments[2] instanceof Uint8Array &&
    (topic.arguments[2] as Uint8Array).join(",") === "1,2,3";

  const event = { type: "sentinel-event" };
  first.emitEvent(event);
  observation.topicSubscription = receivedEvent === event;

  second.emitNext(2);
  document.getElementById("replace")!.click();
  await waitFor(
    () =>
      current?.version === 2 &&
      first.listeners.size === 0 &&
      first.events.size === 0 &&
      second.listeners.size === 1 &&
      second.events.size === 1,
  );
  observation.replacement = true;

  const missingHost = document.createElement("div");
  document.body.append(missingHost);
  const missingRoot = createRoot(missingHost);
  missingRoot.render(
    <ErrorBoundary>
      <MissingProviderProbe />
    </ErrorBoundary>,
  );
  await waitFor(
    () => missingProviderMessage === "useAgent requires AgentProvider",
  );
  observation.missingProvider = true;
  missingRoot.unmount();
  missingHost.remove();

  await waitFor(
    () => playbackFirst.listeners.size === 1 && playbackVideo !== null,
  );
  const canvas = document.createElement("canvas");
  const [firstTrack] = canvas.captureStream().getVideoTracks();
  const [secondTrack] = canvas.captureStream().getVideoTracks();
  playbackFirst.emitTracks({
    first: {
      publicationId: "first",
      participantId: "participant",
      mid: "0",
      kind: "video",
      paused: false,
      media: firstTrack,
    },
    second: {
      publicationId: "second",
      participantId: "participant",
      mid: "1",
      kind: "video",
      paused: false,
      media: secondTrack,
    },
  });
  await waitFor(() => playbackVideo?.srcObject instanceof MediaStream);
  const stream = playbackVideo!.srcObject as MediaStream;
  const retry = playbackRetry;
  const subscriptions = playbackFirst.subscriptions;
  const playAttemptsBeforeRender = playAttempts;
  document.getElementById("playback-callback")!.click();
  await new Promise((resolve) => setTimeout(resolve, 20));
  observation.playbackRetained =
    playbackVideo!.srcObject === stream &&
    playbackRetry === retry &&
    playbackFirst.subscriptions === subscriptions &&
    playAttempts === playAttemptsBeforeRender;

  rejectPlayback = true;
  document.getElementById("playback-select")!.click();
  await waitFor(() => blockedCallbackVersion === 2);
  observation.playbackSelection =
    stream.getTracks().length === 1 && stream.getTracks()[0] === secondTrack;
  await playbackRetry!();
  observation.playbackLatestCallback = blockedCallbackVersion === 2;
  rejectPlayback = false;

  document.getElementById("playback-element")!.click();
  await waitFor(
    () =>
      playbackVideo?.srcObject instanceof MediaStream &&
      playbackVideo.srcObject !== stream &&
      playbackFirst.listeners.size === 1,
  );
  const replacementStream = playbackVideo!.srcObject;
  playbackSecond.emitTracks(playbackFirst.snapshot.tracks);
  document.getElementById("playback-agent")!.click();
  await waitFor(
    () =>
      playbackFirst.listeners.size === 0 &&
      playbackSecond.listeners.size === 1 &&
      playbackVideo?.srcObject instanceof MediaStream,
  );
  observation.playbackReplacement =
    replacementStream !== stream &&
    playbackVideo!.srcObject !== replacementStream;

  const playbackStrictModeReplayed =
    playbackFirst.subscriptions > 1 && playbackFirst.unsubscriptions > 0;
  playbackRoot.unmount();
  playbackHost.remove();
  HTMLMediaElement.prototype.play = originalPlay;
  observation.playbackUnmount =
    playbackFirst.listeners.size === 0 && playbackSecond.listeners.size === 0;
  observation.playbackStrictMode =
    playbackStrictModeReplayed &&
    playbackFirst.subscriptions === playbackFirst.unsubscriptions &&
    playbackSecond.subscriptions === playbackSecond.unsubscriptions;

  const strictModeReplayed =
    first.subscriptions > 1 &&
    first.unsubscriptions > 0 &&
    first.eventSubscriptions > 1 &&
    first.eventUnsubscriptions > 0;
  root.unmount();
  observation.unmount =
    first.listeners.size === 0 &&
    first.events.size === 0 &&
    second.listeners.size === 0 &&
    second.events.size === 0;
  observation.strictMode =
    strictModeReplayed &&
    first.subscriptions === first.unsubscriptions &&
    first.eventSubscriptions === first.eventUnsubscriptions &&
    second.subscriptions === second.unsubscriptions &&
    second.eventSubscriptions === second.eventUnsubscriptions;
  observation.callerOwned = first.closed === 0 && second.closed === 0;
  globalThis.__pulsebeamReactObservation = observation;
})();
