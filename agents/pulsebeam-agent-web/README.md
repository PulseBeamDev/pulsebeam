# PulseBeam Web

`@pulsebeam/web` is the browser-facing declarative PulseBeam agent package.

```ts
import { createAgent } from "@pulsebeam/web";

const agent = createAgent();

agent.setState({
  connection: { roomId: "standup", token: "token" },
  publish: [camera],
  subscribe: [{ participantId: "speaker", kind: "audio", label: "microphone" }],
});

const snapshot = agent.getSnapshot();
const unsubscribe = agent.subscribe(() => render(agent.getSnapshot()));

agent.close();
unsubscribe();
```

`createAgent()` is synchronous. Package evaluation begins private WASM
initialization, but callers neither await it nor receive its result. Snapshots
are immutable and retain their identity until public state changes. `setState`
replaces the complete intent; omitted `publish` and `subscribe` are empty, and
the supplied connection record and arrays are copied. `close()` is terminal.

## Current transport status

Transport reconciliation is deliberately stubbed pending the signaling redesign.
A non-null connection intent immediately reports `connecting`, then deterministically
reports `failed` after WASM initialization settles. A null connection reports an
empty `disconnected` snapshot. Publication, subscription, and data-track intent
does not create remote state or transport media/data in this milestone.

The Rust browser runtime and generated WASM bindings remain private implementation
details. This package does not expose endpoint configuration, topology, commands,
topics, statistics, logging, or generated bindings.

## Development

`just --justfile agents/pulsebeam-agent-web/Justfile check` checks the private
WASM package and TypeScript boundary. `just --justfile agents/pulsebeam-agent-web/Justfile test`
builds the package and runs retained runtime and browser checks.
