import assert from "node:assert/strict";
import { Component, useEffect, useState, type ReactNode } from "react";
import { createRoot } from "react-dom/client";
import { Agent, AgentError, AgentStatus, ErrorCode } from "@pulsebeam/web";
import { useAgent } from "../../src/index.js";
import { flush } from "./platform.js";

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
  assert.ok(failure && AgentError.Operation.instanceOf(failure));
  assert.equal(failure.inner.info.code, ErrorCode.InvalidArgument);
  await flush(() => root.unmount());
}

export async function agentObservationSetupFailure() {
  const expected = AgentError.Operation.new({
    info: {
      code: ErrorCode.CapacityExceeded,
      message: "Agent observation capacity exhausted",
    },
  });
  let observed: Agent | undefined;
  let failure: { error: Error; closed: boolean } | undefined;
  const updates = Agent.prototype.updates;
  Agent.prototype.updates = function () {
    observed = this;
    throw expected;
  };
  function Room() {
    const agent = useAgent({ endpoint: "http://localhost", token: "observer" });
    return agent ? <output>Active Agent</output> : null;
  }
  const root = createRoot(document.createElement("div"));
  try {
    await flush(() =>
      root.render(
        <Boundary
          onError={(error) => {
            failure = {
              error,
              closed: observed?.state().status === AgentStatus.Closed,
            };
          }}
        >
          <Room />
        </Boundary>,
      ),
    );
    assert.deepEqual(failure, { error: expected, closed: true });
  } finally {
    await flush(() => root.unmount());
    Agent.prototype.updates = updates;
  }
}

export async function repeatedSessionFailure() {
  const failures: Error[] = [];
  let current: Agent | null = null;
  let rejectAndReplace!: () => void;
  let returnToFirst!: () => void;
  function Room() {
    const [endpoint, setEndpoint] = useState("http://localhost/first");
    const [token, setToken] = useState("valid");
    const agent = useAgent({ endpoint, token });
    current = agent;
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
  const first = current!;
  await flush(rejectAndReplace);
  assert.equal(host.textContent, "http://localhost/second");
  assert.equal(first.state().status, AgentStatus.Closed);
  await flush(returnToFirst);
  assert.equal(host.textContent, "http://localhost/first");
  assert.deepEqual(
    failures,
    [],
    "a retired lifetime's rejection cannot fail a reused session key",
  );
  await flush(() => root.unmount());
}
