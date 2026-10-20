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
  useAgent,
  type Agent,
  type TopicConsumerError,
  type TopicEvent,
} from "../../src/index.js";
import { createCaptureSource } from "@pulsebeam/web";
import { publishableVideoSource } from "../../src/sources.js";
import { Track, flush } from "./platform.js";
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
export async function relationships() {
  let agent!: Agent;
  let update!: (
    patch: Partial<{
      enabled: boolean;
      source: boolean;
      consumer: boolean;
      second: boolean;
      duplicate: boolean;
      capacity: boolean;
      callback: number;
      speculative: boolean;
    }>,
  ) => void;
  const track = new Track("video");
  const source = publishableVideoSource(
    { kind: "video" },
    createCaptureSource(track as unknown as MediaStreamTrack, "video"),
  );
  const messages: {
    consumer: string;
    event: TopicEvent<number>;
    callback: number;
  }[] = [];
  const errors: TopicConsumerError[] = [];
  let ownershipError: Error | undefined;
  let capacityError: Error | undefined;
  let speculative = false;
  const never = new Promise<never>(() => {});
  function Pending(): never {
    speculative = true;
    throw never;
  }
  function Room() {
    agent = useAgent({
      endpoint: "http://localhost",
      token: "token",
      topology: { localVideos: 1 },
    });
    const [state, setState] = useState({
      enabled: true,
      source: true,
      consumer: true,
      second: true,
      duplicate: false,
      capacity: false,
      callback: 0,
      speculative: false,
    });
    update = (patch) => setState((prior) => ({ ...prior, ...patch }));
    const Camera = agent.local.video("camera");
    const Extra = agent.local.video("extra");
    const Chat = agent.topic<number>("chat", { mode: "ordered" });
    assert.equal(Camera, agent.local.video("camera"));
    assert.equal(Chat, agent.topic<number>("chat", { mode: "ordered" }));
    if (state.speculative)
      for (let i = 0; i < 50; i++) agent.local.video(`abandoned-${i}`);
    const callback = state.callback;
    return (
      <>
        <Camera
          source={state.source ? source : null}
          enabled={state.enabled}
          preferences={{ content: "detail" }}
        />
        {state.consumer && (
          <Chat
            onEvent={(event) =>
              messages.push({ consumer: "one", event, callback })
            }
            onError={(error) => errors.push(error)}
          />
        )}
        {state.second && (
          <Chat
            onEvent={(event) =>
              messages.push({ consumer: "two", event, callback })
            }
            onError={(error) => errors.push(error)}
          />
        )}
        <Boundary
          onError={(error) => {
            ownershipError = error;
          }}
        >
          {state.duplicate && <Camera source={null} />}
        </Boundary>
        <Boundary
          onError={(error) => {
            capacityError = error;
          }}
        >
          {state.capacity && <Extra source={null} />}
        </Boundary>
        {state.speculative && <Pending />}
      </>
    );
  }
  const host = document.createElement("div");
  const root = createRoot(host);
  await flush(() =>
    root.render(
      <StrictMode>
        <Suspense fallback={null}>
          <Room />
        </Suspense>
      </StrictMode>,
    ),
  );
  const runtime = runtimes.at(-1)!;
  assert.equal(runtime.desired.publications.length, 1);
  assert.equal(runtime.desired.topics[0].subscribe, true);
  const desired = runtime.desired;
  await flush(() => update({ callback: 1 }));
  assert.equal(
    runtime.desired,
    desired,
    "callback-only and equal props are external no-ops",
  );
  let sequence = 0n;
  const event = (value: Uint8Array) =>
    runtime.event({
      type: "topic-message",
      mode: "ordered",
      topic: "chat",
      participantId: "alice",
      dataTrackId: "remote-chat",
      sequence: sequence++,
      payload: value,
    });
  await flush(() => event(new TextEncoder().encode("7")));
  assert.deepEqual(
    messages,
    ["one", "two"].map((consumer) => ({
      consumer,
      event: {
        type: "message",
        topic: "chat",
        value: 7,
        participantId: "alice",
        dataTrackId: "remote-chat",
        sequence: 0n,
      },
      callback: 1,
    })),
  );
  await flush(() =>
    startTransition(() => update({ callback: 2, speculative: true })),
  );
  assert.equal(speculative, true);
  await flush(() => event(new TextEncoder().encode("8")));
  assert.ok(
    messages.slice(-2).every((message) => message.callback === 1),
    "speculative callbacks never become live",
  );
  await flush(() =>
    update({ speculative: false, callback: 1, enabled: false }),
  );
  assert.equal(
    runtime.desired.publications.length,
    1,
    "mute retains publication",
  );
  assert.equal(track.readyState, "live", "Agent borrows capture");
  await flush(() => update({ source: false }));
  assert.equal(
    runtime.desired.publications.length,
    0,
    "null source withdraws publication",
  );
  await flush(() => update({ duplicate: true }));
  assert.equal(
    runtime.desired.publications.length,
    0,
    "null source still owns the declaration",
  );
  await flush(() => update({ source: true, enabled: true, capacity: true }));
  assert.equal(capacityError?.name, "LocalTrackCapacityError");
  assert.equal(ownershipError?.name, "RelationshipOwnershipError");
  assert.equal(
    runtime.desired.publications.length,
    1,
    "duplicate cannot steal or withdraw first owner",
  );
  await flush(() => update({ consumer: false }));
  await flush(() => event(new TextEncoder().encode("9")));
  assert.equal(messages.at(-1)!.consumer, "two");
  assert.equal(
    messages.filter(
      (message) =>
        message.event.type === "message" && message.event.value === 9,
    ).length,
    1,
  );
  const gap = {
    type: "topic-recovery-gap",
    topic: "chat",
    participantId: "alice",
    dataTrackId: "remote-chat",
    expectedSequence: 3n,
    availableSequence: 5n,
  };
  await flush(() => update({ consumer: true }));
  await flush(() => runtime.event(gap));
  assert.deepEqual(
    messages.slice(-2).map(({ event }) => event),
    [0, 1].map(() => ({ ...gap, type: "gap" })),
  );
  sequence = 5n;
  await flush(() => event(new TextEncoder().encode("10")));
  assert.ok(
    messages
      .slice(-2)
      .every(({ event }) => event.type === "message" && event.sequence === 5n),
  );
  const channelFailure = {
    type: "topic-channel-failed",
    direction: "subscribe",
    topic: "chat",
    mode: "ordered",
    message: "core failure",
  };
  await flush(() => runtime.event(channelFailure));
  assert.deepEqual(errors.at(-1), {
    code: "channel-failed",
    event: channelFailure,
  });
  const fencedCount = messages.length;
  await flush(() => event(new Uint8Array([0xff])));
  assert.equal(
    errors.filter((error) => error.code === "decode-failed").length,
    0,
    "channel failure fenced both consumers",
  );
  assert.equal(messages.length, fencedCount);
  await flush(() => update({ consumer: false, second: false }));
  await flush(() => update({ consumer: true, second: true }));
  await flush(() => event(new Uint8Array([0xff])));
  assert.equal(
    errors.filter((error) => error.code === "decode-failed").length,
    2,
  );
  assert.equal(
    runtime.desired.topics.length,
    0,
    "decode failure releases affected consumers",
  );
  const Chat = agent.topic<number>("chat", { mode: "ordered" });
  agent.connect();
  const admission = Chat.publish(10);
  await flush(() =>
    runtime.emit({
      connection: "connected",
      generation: 1n,
      topics: {
        ...runtime.snapshot.topics,
        publishers: [
          {
            name: "chat",
            mode: "ordered",
            connected: true,
            dataTrackId: "local-chat",
            nextSequence: 0n,
            acceptedHistory: 0,
            queued: 0,
            sendPending: false,
          },
        ],
      },
    }),
  );
  await admission;
  assert.equal(runtime.desired.topics[0].publish, true);
  assert.equal(
    runtime.desired.topics[0].subscribe,
    false,
    "publishing needs no subscribers",
  );
  await flush(() => track.end());
  assert.equal(
    runtime.desired.publications.length,
    0,
    "ended borrowed source cannot remain published",
  );
  assert.equal(track.stopped, 0, "publication does not stop caller capture");
  await flush(() => root.unmount());
  assert.equal(track.stopped, 0);
  const count = messages.length;
  await flush(() => event(new TextEncoder().encode("11")));
  assert.equal(messages.length, count, "unmounted consumers cannot receive");
  track.stop();
}
