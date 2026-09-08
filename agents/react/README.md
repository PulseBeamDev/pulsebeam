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

const agent = createAgent();

function Status() {
  const { connection, setState } = useAgent();
  return (
    <button onClick={() => setState({ connection: null })}>
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

The current web agent has a signaling stub: a non-null connection intent moves
from `connecting` to `failed` after initialization, while publication,
subscription, and data-track intent do not yet create transport state.
