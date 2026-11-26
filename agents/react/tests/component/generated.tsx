import assert from "node:assert/strict";
import {
  Component,
  StrictMode,
  Suspense,
  startTransition,
  useState,
  type ReactNode,
} from "react";
import { createRoot } from "react-dom/client";
import {
  Agent,
  AgentStatus,
  AudioPublication,
  AudioSubscription,
  Catalog,
  DataDelivery,
  DataPublication,
  DataSubscription,
  LocalVideoTrack,
  PipeStatus,
  VideoPublication,
  VideoSource,
  VideoSubscription,
  WatchStatus,
  type Pipe,
} from "@pulsebeam/web";
import {
  useAgent,
  usePublish,
  useSubscribe,
  useWatch,
} from "../../src/index.js";
import { playbackLifetimes } from "./video.js";
import { concurrentRender } from "./concurrency.js";
import { dataLifetimes } from "./data.js";
import { watchLifecycle } from "./watch.js";
import {
  agentCreationFailure,
  agentObservationSetupFailure,
  repeatedSessionFailure,
} from "./ownership.js";
import { captureLifecycle } from "./capture.js";
import { flush, Track } from "./platform.js";

async function handles() {
  const inputTrack = new Track("video") as unknown as MediaStreamTrack;
  const replacementTrack = new Track("video") as unknown as MediaStreamTrack;
  const inputs = [
    VideoSource.fromTrack(inputTrack),
    VideoSource.fromTrack(replacementTrack),
  ];
  const pipes: Pipe[] = [];
  for (const input of inputs) {
    const pipeTo = input.pipeTo.bind(input);
    input.pipeTo = (publication) => {
      const pipe = pipeTo(publication);
      pipes.push(pipe);
      return pipe;
    };
  }
  let created: Agent | undefined;
  let renew!: (token: string) => void;
  let setOptions!: (next: {
    enabled: boolean;
    high: boolean;
    room: string;
    source: number | null;
    mode: DataDelivery;
  }) => void;
  const initial = {
    enabled: true,
    high: false,
    room: "first",
    source: 0,
    mode: DataDelivery.Reliable,
  };
  let handles: ReturnType<typeof useOwners> | undefined;
  let rendered = 0;
  let renderedAbsent = false;
  let renderedBeforeAgent = false;
  let renderedBeforeOwners = false;

  function useOwners(
    agent: Agent,
    options: typeof initial | Parameters<typeof setOptions>[0],
  ) {
    const room = agent.room(options.room);
    const peer = room.participant("peer");
    const watch = useWatch(room);
    const publication = usePublish(
      agent.video("camera"),
      options.source === null ? null : inputs[options.source],
      { enabled: options.enabled },
    );
    const video = useSubscribe(peer.video("camera"), {
      visible: true,
      height: options.high ? 720 : 360,
      minHeight: 0,
      minFps: 0,
      priority: 1,
    });
    const audio = useSubscribe(peer.audio("microphone"));
    const cameras = useSubscribe(room.video({ trackId: "camera" }));
    const microphone = usePublish(agent.audio("microphone"));
    const events = usePublish(agent.data("events"), null, {
      delivery: options.mode,
    });
    const receive = useSubscribe(peer.data("events"), {
      delivery: options.mode,
    });
    return { watch, publication, video, audio, cameras, microphone, events, receive };
  }

  function Owners({ agent }: { agent: Agent }) {
    const [options, set] = useState<Parameters<typeof setOptions>[0]>(initial);
    setOptions = set;
    rendered++;
    handles = useOwners(agent, options);
    if (Object.values(handles).every((owner) => owner === null))
      renderedBeforeOwners = true;
    if (options.room === "second" && handles.watch === null)
      renderedAbsent = true;
    return null;
  }

  function App() {
    const [token, setToken] = useState("initial");
    renew = setToken;
    const agent = useAgent({ endpoint: "http://localhost", token });
    if (!agent) {
      renderedBeforeAgent = true;
      return null;
    }
    created = agent;
    return <Owners agent={agent} />;
  }

  const root = createRoot(document.createElement("div"));
  try {
    await flush(() =>
      root.render(
        <StrictMode>
          <App />
        </StrictMode>,
      ),
    );
    assert.ok(renderedBeforeAgent && renderedBeforeOwners);
    assert.ok(Agent.instanceOf(created));
    assert.equal(created.state().status, AgentStatus.Idle, "creation is inert");
    const first = handles!;
    assert.ok(Catalog.instanceOf(first.watch));
    assert.ok(VideoPublication.instanceOf(first.publication));
    assert.ok(VideoSubscription.instanceOf(first.video));
    assert.ok(AudioPublication.instanceOf(first.microphone));
    assert.ok(AudioSubscription.instanceOf(first.audio));
    assert.ok(DataPublication.instanceOf(first.events));
    assert.ok(DataSubscription.instanceOf(first.receive));
    assert.equal(first.watch.state(), first.watch.state());
    assert.equal(first.publication.state(), first.publication.state());
    const before = rendered;
    const firstAgentState = created.state();
    assert.equal(created.state(), firstAgentState);
    await flush(() => renew("renewed"));
    assert.equal(created.state().status, AgentStatus.Idle);
    assert.ok(
      created.state().observationRevision > firstAgentState.observationRevision,
    );
    assert.ok(rendered > before);

    const firstPipe = pipes.at(-1)!;
    assert.ok(firstPipe);
    const preferences = { ...initial, high: true, enabled: false, source: 1 };
    await flush(() => setOptions(preferences));
    assert.equal(handles!.publication, first.publication);
    assert.equal(handles!.video, first.video);
    assert.equal(first.publication.state().configuration.enabled, false);
    assert.equal(first.video.state().configuration.height, 720);
    assert.equal(firstPipe.state().status, PipeStatus.Closed);
    const secondPipe = pipes.at(-1)!;
    assert.notEqual(secondPipe, firstPipe);
    await flush(() => setOptions({ ...preferences, source: null }));
    assert.equal(secondPipe.state().status, PipeStatus.Closed);
    assert.equal(handles!.publication, first.publication);

    await flush(() =>
      setOptions({
        ...preferences,
        source: null,
        room: "second",
        mode: DataDelivery.Latest,
      }),
    );
    assert.ok(renderedAbsent, "new watch scope never reads an old watch");
    assert.equal(first.watch.state().status, WatchStatus.Closed);
    assert.equal(handles!.watch!.state().room, "second");
    assert.equal(first.video.state().closed, true);
    assert.equal(first.audio.state().closed, true);
    assert.equal(first.events.state().closed, true);
    assert.equal(first.receive.state().closed, true);
    assert.equal(first.publication.state().closed, false);
    assert.equal(inputs[0].previewTrack(), inputTrack);
    assert.equal(inputs[1].previewTrack(), replacementTrack);
  } finally {
    await flush(() => root.unmount());
    assert.equal(created?.state().status, AgentStatus.Closed);
    for (const input of inputs) await input.close();
  }
}

class Boundary extends Component<
  { onFailure(error: unknown): void; children: ReactNode },
  { failed: boolean }
> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  componentDidCatch(error: unknown) {
    this.props.onFailure(error);
  }
  render() {
    return this.state.failed ? null : this.props.children;
  }
}

async function setupFailureAndAbandon() {
  let suspended = false;
  let publications = 0;
  let retired: VideoPublication | undefined;
  const fault = new Error("observer setup failed");
  const busy = new Promise<never>(() => {});
  const publish = LocalVideoTrack.prototype.publish;
  LocalVideoTrack.prototype.publish = function (options) {
    publications++;
    retired = publish.call(this, options);
    retired.updates = () => {
      throw fault;
    };
    return retired;
  };
  function Owner({
    agent,
    speculative,
  }: {
    agent: Agent;
    speculative: boolean;
  }) {
    usePublish(agent.video("camera"));
    if (speculative) {
      suspended = true;
      throw busy;
    }
    return null;
  }
  function App({ show, speculative }: { show: boolean; speculative: boolean }) {
    const agent = useAgent({ endpoint: "http://localhost", token: "opaque" });
    return show && agent ? (
      <Owner agent={agent} speculative={speculative} />
    ) : null;
  }
  const root = createRoot(document.createElement("div"));
  let observed: unknown;
  const render = (show: boolean, speculative: boolean) =>
    root.render(
      <Boundary
        onFailure={(err) => {
          observed = err;
        }}
      >
        <Suspense fallback={null}>
          <App show={show} speculative={speculative} />
        </Suspense>
      </Boundary>,
    );
  try {
    await flush(() => render(false, false));
    await flush(() => startTransition(() => render(true, true)));
    assert.ok(suspended);
    assert.equal(publications, 0, "abandoned render creates no publication");
    await flush(() => render(true, false));
    assert.equal(publications, 1);
    assert.equal(observed, fault);
    assert.ok(
      retired?.state().closed,
      "observer setup failure retires the partial owner",
    );
  } finally {
    await flush(() => root.unmount());
    LocalVideoTrack.prototype.publish = publish;
  }
}

await handles();
await setupFailureAndAbandon();
await captureLifecycle();
await concurrentRender();
await playbackLifetimes();
await dataLifetimes();
await watchLifecycle();
await agentCreationFailure();
await agentObservationSetupFailure();
await repeatedSessionFailure();
