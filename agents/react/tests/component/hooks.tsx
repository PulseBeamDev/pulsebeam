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
  Agent as WebAgent,
  DataDelivery,
  VideoSource,
  type Pipe,
  type VideoPublication,
} from "@pulsebeam/web";
import { useAgent, usePublish, useSubscribe } from "../../src/index.js";
import { flush, Track } from "./platform.js";
import { runtimes } from "./runtime.js";

class Boundary extends Component<
  { children: ReactNode; onError(error: Error): void },
  { failed: boolean }
> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  componentDidCatch(error: Error) {
    this.props.onError(error);
  }
  render() {
    return this.state.failed ? null : this.props.children;
  }
}

export async function actualHandleHooks() {
  const firstTrack = new Track("video");
  const secondTrack = new Track("video");
  const firstSource = VideoSource.fromTrack(
    firstTrack as unknown as MediaStreamTrack,
  );
  const secondSource = VideoSource.fromTrack(
    secondTrack as unknown as MediaStreamTrack,
  );
  const pipes: Pipe[] = [];
  for (const source of [firstSource, secondSource]) {
    const pipeTo = source.pipeTo.bind(source);
    source.pipeTo = (publication: VideoPublication) => {
      const pipe = pipeTo(publication);
      pipes.push(pipe);
      return pipe;
    };
  }
  let setSource!: (source: VideoSource | null) => void;
  let setLabel!: (label: string) => void;
  let setEnabled!: (enabled: boolean) => void;
  let setPresent!: (present: boolean) => void;
  let setHeight!: (height: number) => void;
  let publication: ReturnType<typeof usePublish>;
  let subscription: ReturnType<typeof useSubscribe>;
  const seen: NonNullable<typeof publication>[] = [];
  let renders = 0;
  let initialAgentNull = false;
  let initialOwnersNull = false;

  function Room() {
    const agent = useAgent({ endpoint: "http://localhost", token: "hooks" });
    if (!agent) {
      initialAgentNull = true;
      return null;
    }
    assert.ok(agent instanceof WebAgent);
    return <Owners agent={agent} />;
  }
  function Owners({
    agent,
  }: {
    agent: NonNullable<ReturnType<typeof useAgent>>;
  }) {
    ++renders;
    const [source, changeSource] = useState<VideoSource | null>(firstSource);
    const [label, changeLabel] = useState("camera");
    const [enabled, changeEnabled] = useState(true);
    const [present, changePresent] = useState(true);
    const [height, changeHeight] = useState(360);
    setSource = changeSource;
    setLabel = changeLabel;
    setEnabled = changeEnabled;
    setPresent = changePresent;
    setHeight = changeHeight;
    publication = usePublish(present ? agent.video(label) : null, source, {
      enabled,
    });
    subscription = useSubscribe(
      present ? agent.room("room").participant("alice").video(label) : null,
      {
        visible: true,
        height,
        minHeight: 0,
        minFps: 0,
        priority: 1,
        playoutDelay: { mode: "adaptive" },
      },
    );
    if (!publication && !subscription) initialOwnersNull = true;
    if (publication && !seen.includes(publication)) seen.push(publication);
    return null;
  }
  const root = createRoot(document.createElement("div"));
  try {
    await flush(() =>
      root.render(
        <StrictMode>
          <Room />
        </StrictMode>,
      ),
    );
    assert.ok(
      initialAgentNull,
      "Agent is not exposed during render before its effect",
    );
    assert.ok(
      initialOwnersNull,
      "resource handles are not exposed before effects",
    );
    assert.ok(publication);
    assert.ok(subscription);
    const first = publication;
    const firstSubscription = subscription;
    assert.equal(
      runtimes.at(-1)!.joins.length,
      0,
      "useAgent does not join implicitly",
    );
    const firstPipe = pipes.at(-1);
    assert.ok(firstPipe, "media source is piped to its publication");
    const runtime = runtimes.at(-1)!;
    const keys = [
      "agent",
      [...runtime.publicationOwners.keys()][0]!,
      [...runtime.videoOwners.keys()][0]!,
      [...runtime.observations.keys()].find((key) => typeof key === "object")!,
    ] as const;
    for (const key of keys) {
      const before = renders;
      const state = runtime.observer(key).state();
      await flush(() =>
        runtime.setObservation(key, {
          ...state,
          observationRevision: state.observationRevision + 1n,
        }),
      );
      assert.ok(
        renders > before,
        "Agent, publication, subscription and pipe each independently wake React",
      );
    }
    const snapshot = first.state();
    const before = renders;
    const publicationKey = [...runtime.publicationOwners.keys()][0]!;
    await flush(() =>
      runtime.setObservation(publicationKey, {
        ...runtime.observer(publicationKey).state(),
      }),
    );
    assert.equal(
      first.state(),
      snapshot,
      "rendering and imperative reads share stable state identity",
    );
    assert.equal(renders, before, "the same revision does not rerender");
    await flush(() => setSource(secondSource));
    assert.equal(
      publication,
      first,
      "source replacement preserves publication",
    );
    const secondPipe = pipes.at(-1);
    assert.notEqual(
      secondPipe,
      firstPipe,
      "replacement source creates a new pipe",
    );
    assert.equal(firstPipe.isClosed(), true, "retired source pipe is closed");
    await flush(() => setHeight(720));
    assert.equal(
      subscription,
      firstSubscription,
      "receive options configure the existing subscription",
    );
    await flush(() => setEnabled(false));
    assert.equal(publication, first, "configuration preserves publication");
    assert.equal(
      subscription,
      firstSubscription,
      "unrelated changes preserve subscription",
    );
    await flush(() => setSource(null));
    assert.equal(
      publication,
      first,
      "null source detaches the pipe, not its owner",
    );
    assert.equal(secondPipe!.isClosed(), true, "null source retires its pipe");
    assert.equal(firstSource.isClosed(), false);
    assert.equal(secondSource.isClosed(), false);
    await flush(() => setLabel("other"));
    assert.notEqual(
      publication,
      first,
      "track replacement creates a fresh publication",
    );
    assert.notEqual(
      subscription,
      firstSubscription,
      "track replacement creates a fresh subscription",
    );
    assert.equal(first.isClosed(), true);
    assert.equal(firstSubscription.isClosed(), true);
    const replacement = publication!;
    const replacementSubscription = subscription!;
    await flush(() => setPresent(false));
    assert.equal(publication, null);
    assert.equal(subscription, null);
    assert.equal(replacement.isClosed(), true);
    assert.equal(replacementSubscription.isClosed(), true);
    await flush(() => setPresent(true));
    assert.notEqual(
      publication,
      replacement,
      "remounting ownership creates a fresh handle",
    );
  } finally {
    await flush(() => root.unmount());
    assert.ok(
      seen.every((handle) => handle.isClosed()),
      "unmount closes every exposed owner",
    );
    assert.equal(firstSource.isClosed(), false, "borrowed sources remain open");
    assert.equal(
      secondSource.isClosed(),
      false,
      "replacement source remains open",
    );
    await firstSource.close();
    await secondSource.close();
  }
}

export async function abandonedHookOwners() {
  const never = new Promise<void>(() => {});
  let suspended = false;
  function Room({ speculative }: { speculative: boolean }) {
    const agent = useAgent({
      endpoint: "http://localhost",
      token: "abandoned-hooks",
    });
    return agent && speculative ? <Owners agent={agent} /> : null;
  }
  function Owners({
    agent,
  }: {
    agent: NonNullable<ReturnType<typeof useAgent>>;
  }) {
    usePublish(agent.video("abandoned"), null);
    useSubscribe(agent.room("room").participant("alice").video("abandoned"));
    suspended = true;
    throw never;
  }
  const root = createRoot(document.createElement("div"));
  try {
    await flush(() =>
      root.render(
        <Suspense fallback={null}>
          <Room speculative={false} />
        </Suspense>,
      ),
    );
    const runtime = runtimes.at(-1)!;
    const before = runtime.resourceCommands.length;
    await flush(() =>
      startTransition(() =>
        root.render(
          <Suspense fallback={null}>
            <Room speculative />
          </Suspense>,
        ),
      ),
    );
    assert.equal(suspended, true);
    assert.equal(
      runtime.resourceCommands.length,
      before,
      "abandoned renders allocate no owners",
    );
  } finally {
    await flush(() => root.unmount());
  }
}

export async function audioAndDataHookOwners() {
  let audio: import("@pulsebeam/web").AudioPublication | null = null;
  let data: import("@pulsebeam/web").DataPublication | null = null;
  let audioReceive: import("@pulsebeam/web").AudioSubscription | null = null;
  let dataReceive: import("@pulsebeam/web").DataSubscription | null = null;
  function Room({ delivery }: { delivery?: DataDelivery }) {
    const agent = useAgent({
      endpoint: "http://localhost",
      token: "audio-data-hooks",
    });
    audio = usePublish(agent?.audio("voice") ?? null);
    const options = delivery === undefined ? undefined : { delivery };
    data = usePublish(agent?.data("events") ?? null, null, options);
    const peer = agent?.room("room").participant("alice");
    audioReceive = useSubscribe(peer?.audio("voice") ?? null);
    dataReceive = useSubscribe(peer?.data("events") ?? null, options);
    return null;
  }
  const root = createRoot(document.createElement("div"));
  try {
    await flush(() =>
      root.render(
        <StrictMode>
          <Room />
        </StrictMode>,
      ),
    );
    for (const owner of [audio, data, audioReceive, dataReceive]) {
      assert.ok(owner);
      assert.equal(owner instanceof Promise, false);
      assert.equal(owner.state(), owner.state());
    }
    const reliablePublication =
      data! as import("@pulsebeam/web").DataPublication;
    const reliableSubscription =
      dataReceive! as import("@pulsebeam/web").DataSubscription;
    const render = (delivery: DataDelivery) =>
      flush(() =>
        root.render(
          <StrictMode>
            <Room delivery={delivery} />
          </StrictMode>,
        ),
      );
    await render(DataDelivery.Reliable);
    assert.equal(data, reliablePublication);
    assert.equal(dataReceive, reliableSubscription);
    await render(DataDelivery.Unreliable);
    assert.notEqual(data, reliablePublication);
    assert.notEqual(dataReceive, reliableSubscription);
    assert.equal(reliablePublication.isClosed(), true);
    assert.equal(reliableSubscription.isClosed(), true);
    assert.equal(
      [...runtimes.at(-1)!.dataOwners.values()][0].spec.delivery,
      "lossy",
    );
  } finally {
    await flush(() => root.unmount());
  }
  for (const owner of [audio, data, audioReceive, dataReceive])
    assert.equal(owner!.isClosed(), true);
}

export async function hookSetupFailure() {
  const track = new Track("video");
  const source = VideoSource.fromTrack(track as unknown as MediaStreamTrack);
  await source.close();
  let failure: Error | undefined;
  function Room() {
    const agent = useAgent({
      endpoint: "http://localhost",
      token: "hook-failure",
    });
    return agent ? <Owner agent={agent} /> : null;
  }
  function Owner({
    agent,
  }: {
    agent: NonNullable<ReturnType<typeof useAgent>>;
  }) {
    usePublish(agent.video("failed"), source);
    return null;
  }
  const root = createRoot(document.createElement("div"));
  try {
    await flush(() =>
      root.render(
        <Boundary onError={(error) => (failure = error)}>
          <Room />
        </Boundary>,
      ),
    );
    assert.ok(failure, "partial pipe setup failure reaches an Error Boundary");
    assert.equal(
      runtimes.at(-1)!.publicationOwners.size,
      0,
      "failed setup releases the publication",
    );
  } finally {
    await flush(() => root.unmount());
  }
}
