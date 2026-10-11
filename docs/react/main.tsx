import { useState } from "react";
import { createRoot } from "react-dom/client";
import type { AgentConfig } from "@pulsebeam/react";
import { Room } from "./Room.js";

function App() {
  const [config, setConfig] = useState<AgentConfig | null>(null);
  return config ? (
    <>
      <Room config={config} />
      <button onClick={() => setConfig(null)}>Unmount room</button>
    </>
  ) : (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        const fields = new FormData(event.currentTarget);
        setConfig({
          endpoint: String(fields.get("endpoint")),
          token: String(fields.get("token")),
          topology: {
            localVideos: 1,
            localAudios: 1,
            remoteVideos: 2,
            remoteAudios: 2,
          },
        });
      }}
    >
      <h1>Configure room</h1>
      <label>
        Server endpoint{" "}
        <input name="endpoint" defaultValue="http://localhost:7070" required />
      </label>
      <label>
        Participant token{" "}
        <input name="token" type="password" required autoComplete="off" />
      </label>
      <button>Open room</button>
    </form>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
