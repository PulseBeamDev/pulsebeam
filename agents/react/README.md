# PulseBeam React

`@pulsebeam/react` connects a caller-owned `@pulsebeam/web` agent to React.

Install the adapter with React and the web agent:

```sh
pnpm add @pulsebeam/react @pulsebeam/web react
```

Create and close the agent outside the adapter, then place `AgentProvider` above
components that call `useAgent`. The provider never initializes, replaces, or
closes its agent.

```tsx
import { AgentProvider, useAgent } from "@pulsebeam/react";
import { createAgent } from "@pulsebeam/web";

const agent = createAgent({
  endpoint: "https://pulsebeam.example",
  roomId: "standup",
  topology: {},
});

function Status() {
  const { connection, setState } = useAgent();
  return (
    <button onClick={() => setState({ connected: false })}>
      {connection}
    </button>
  );
}

function App() {
  return (
    <AgentProvider agent={agent}>
      <Status />
    </AgentProvider>
  );
}

// Call agent.close() when the caller's lifecycle ends.
```

The web agent owns the browser runtime. The React adapter only subscribes to
its immutable snapshots and forwards complete desired state updates.
