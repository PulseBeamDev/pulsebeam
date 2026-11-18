import assert from "node:assert/strict";
import {
  Component,
  StrictMode,
  useEffect,
  useState,
  type ReactNode,
} from "react";
import { createRoot } from "react-dom/client";
import {
  Audio,
  Data,
  Publish,
  SelectAudio,
  useAgent,
  type Agent,
  type DataEvent,
} from "../../src/index.js";
import {
  VideoSource,
  ResourceOwnershipError,
  OperationError,
  ErrorCode,
} from "@pulsebeam/web";
import { publishableVideoSource } from "../../src/sources.js";
import { createReactAgent, managedAgent } from "../../src/agent.js";
import { deferred, flush, Track } from "./platform.js";
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

export async function agentCreationFailure() {
  let failure: Error | undefined;
  function InvalidAgent() {
    const agent = useAgent({ endpoint: "http://localhost", token: " " });
    return agent ? <output>unexpected Agent</output> : null;
  }
  const root = createRoot(document.createElement("div"));
  await flush(() =>
    root.render(
      <Boundary onError={(error) => (failure = error)}>
        <InvalidAgent />
      </Boundary>,
    ),
  );
  assert.ok(failure instanceof OperationError);
  assert.equal(failure.info.code, ErrorCode.InvalidArgument);
  assert.equal(Object.isFrozen(failure.info), true);
  await flush(() => root.unmount());
}

export async function repeatedSessionFailure() {
  const failures: Error[] = [];
  let rejectAndReplace!: () => void;
  let returnToFirst!: () => void;
  function Room() {
    const [endpoint, setEndpoint] = useState("http://localhost/first");
    const [token, setToken] = useState("valid");
    const agent = useAgent({ endpoint, token });
    useEffect(() => {
      if (token === " ") {
        setEndpoint("http://localhost/second");
        setToken("valid");
      }
    }, [token]);
    rejectAndReplace = () => setToken(" ");
    returnToFirst = () => setEndpoint("http://localhost/first");
    return agent ? <output>{endpoint}</output> : null;
  }
  const host = document.createElement("div");
  const root = createRoot(host);
  await flush(() =>
    root.render(
      <Boundary onError={(error) => failures.push(error)}>
        <Room />
      </Boundary>,
    ),
  );
  const first = runtimes.at(-1)!;
  await flush(rejectAndReplace);
  assert.equal(host.textContent, "http://localhost/second");
  assert.equal(first.closed, true);
  await flush(returnToFirst);
  assert.equal(host.textContent, "http://localhost/first");
  assert.deepEqual(
    failures,
    [],
    "a retired lifetime's rejection cannot fail a reused session key",
  );
  await flush(() => root.unmount());
}

export async function stalePublicationClaim() {
  let replace!: () => void;
  const completion = deferred<void>();
  const native = new Track("video");
  const source = publishableVideoSource(
    { kind: "video" },
    VideoSource.fromTrack(native as unknown as MediaStreamTrack),
  );
  let activate!: () => void;
  function Room() {
    const agent = useAgent({
      endpoint: "http://localhost",
      token: "stale-publication",
    });
    return agent ? <StalePublicationRoom agent={agent} /> : null;
  }
  function StalePublicationRoom({ agent }: { agent: Agent }) {
    const [label, setLabel] = useState("old");
    const [mounted, setMounted] = useState(false);
    activate = () => setMounted(true);
    replace = () => setLabel("new");
    return mounted ? (
      <Publish track={agent.video(label)} source={source} />
    ) : null;
  }
  const root = createRoot(document.createElement("div"));
  await flush(() => root.render(<Room />));
  const runtime = runtimes.at(-1)!;
  const resource = runtime.resource.bind(runtime);
  runtime.resource = async (command) => {
    if (command.op === "claimPublication" && command.spec.label === "old")
      await completion.promise;
    return resource(command);
  };
  await flush(activate);
  await flush(replace);
  assert.equal(
    runtime.publicationOwners.size,
    0,
    "replacement waits for the pending retired claim to close",
  );
  await flush(() => completion.resolve());
  const owner = [...runtime.publicationOwners.keys()][0];
  assert.notEqual(owner, undefined);
  assert.deepEqual([...runtime.claimedPublications], ["video:new"]);
  assert.deepEqual([...runtime.publicationOwners.keys()], [owner]);
  assert.equal(runtime.ownedReplacements[0].replaceInput, true);
  assert.ok(
    runtime.ownedReplacements.every(
      (entry) => entry.owner === owner && entry.muted === false,
    ),
    "retired claim cannot mutate the replacement publication",
  );
  assert.equal(
    native.readyState,
    "live",
    "publication keeps borrowed capture alive",
  );
  await flush(() => root.unmount());
  assert.equal(runtime.publicationOwners.size, 0);
  assert.equal(native.readyState, "live");
}

export async function ownershipAdapters() {
  let agent!: Agent;
  let update!: (
    patch: Partial<{
      source: boolean;
      enabled: boolean;
      duplicate: boolean;
      selection: boolean;
    }>,
  ) => void;
  let duplicateFailure: Error | undefined;
  let audioFailure: Error | undefined;
  const events: DataEvent<"ordered">[] = [];
  const native = new Track("video");
  const source = publishableVideoSource(
    { kind: "video" },
    VideoSource.fromTrack(native as unknown as MediaStreamTrack),
  );

  function Room() {
    const current = useAgent({
      endpoint: "http://localhost",
      token: "ownership",
    });
    return current ? <OwnershipRoom agent={current} /> : null;
  }
  function OwnershipRoom({ agent: current }: { agent: Agent }) {
    agent = current;
    const [state, setState] = useState({
      source: false,
      enabled: true,
      duplicate: false,
      selection: false,
    });
    update = (patch) => setState((before) => ({ ...before, ...patch }));
    // Distinct SDK wrappers still denote the same Web-interned value.
    const equivalent = createReactAgent(managedAgent(agent));
    const camera = equivalent.video("camera");
    const messages = equivalent.data("messages", "ordered");
    return (
      <>
        <Audio agent={equivalent} />
        {state.selection && (
          <SelectAudio
            agent={equivalent}
            policy={{ automatic: state.enabled }}
          />
        )}
        <Data
          track={messages}
          onEvent={(event) => {
            events.push(event);
          }}
        />
        <Publish
          track={camera}
          source={state.source ? source : null}
          enabled={state.enabled}
          content="text"
        />
        {state.duplicate && (
          <>
            <Boundary onError={(error) => (duplicateFailure = error)}>
              <Publish track={camera} source={source} />
            </Boundary>
            <Boundary onError={(error) => (audioFailure = error)}>
              <Audio agent={equivalent} />
            </Boundary>
          </>
        )}
      </>
    );
  }

  const host = document.createElement("div");
  const root = createRoot(host);
  await flush(() =>
    root.render(
      <StrictMode>
        <Room />
      </StrictMode>,
    ),
  );
  const runtime = runtimes.at(-1)!;
  assert.equal(
    runtime.claimedPublications.size,
    0,
    "initial null waits for a concrete source",
  );
  assert.equal(
    runtime.resourceCommands.some(
      (command) => command.op === "claimAudioPolicy",
    ),
    false,
    "playback does not create selection demand",
  );
  await flush(() => update({ source: true }));
  assert.equal(runtime.claimedPublications.size, 1);
  const publicationOwner = [...runtime.publicationOwners.keys()][0];
  const dataOwner = [...runtime.dataOwners.keys()][0];
  const audioOwner = runtime.audioOwner;
  assert.deepEqual(Object.keys(agent.video("camera")), ["kind", "label"]);
  assert.equal("track" in agent, false);
  assert.equal("track" in agent.participant("alice"), false);
  assert.notEqual(runtime.audioOwner, undefined);
  assert.equal(runtime.dataOwners.size, 1);
  runtime.deliverData("messages", {
    type: "message",
    participantId: "alice",
    trackId: "message-id",
    sequence: 1n,
    data: new Uint8Array([0, 255]),
  });
  await flush();
  assert.equal(events.length, 1);
  assert.deepEqual(events[0]?.data, new Uint8Array([0, 255]));
  assert.equal(runtime.ownedReplacements.at(-1)?.track, native);
  assert.equal(runtime.ownedReplacements.at(-1)?.config.contentHint, "text");

  await flush(() => update({ enabled: false }));
  assert.ok([...runtime.ownedMuted.values()].includes(true));
  assert.deepEqual([...runtime.publicationOwners.keys()], [publicationOwner]);
  assert.deepEqual([...runtime.dataOwners.keys()], [dataOwner]);
  assert.equal(runtime.audioOwner, audioOwner);
  await flush(() => update({ source: false }));
  assert.equal(runtime.ownedReplacements.at(-1)?.track, null);
  assert.equal(native.readyState, "live");

  await flush(() => update({ source: true, duplicate: true, selection: true }));
  await flush();
  assert.ok(duplicateFailure instanceof ResourceOwnershipError);
  assert.ok(audioFailure instanceof ResourceOwnershipError);
  assert.equal(runtime.audioOwner, audioOwner);
  assert.equal(runtime.claimedPublications.size, 1);
  assert.equal(runtime.audioSelections.size, 1);
  const selection = [...runtime.audioSelections.keys()][0]!;
  assert.equal(runtime.audioSelections.get(selection).automatic, false);
  await flush(() => update({ enabled: true }));
  assert.deepEqual([...runtime.audioSelections.keys()], [selection]);
  assert.equal(runtime.audioSelections.get(selection).automatic, true);
  await flush(() => update({ selection: false }));
  assert.equal(runtime.audioSelections.size, 0);
  assert.equal(
    runtime.audioOwner,
    audioOwner,
    "selection withdrawal does not close playback",
  );

  await flush(() => root.unmount());
  assert.equal(runtime.claimedPublications.size, 0);
  assert.equal(runtime.audioOwner, undefined);
  assert.equal(runtime.dataOwners.size, 0);
  assert.equal(native.readyState, "live");
}
