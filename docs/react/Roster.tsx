import { useMemo, useState } from "react";
import { useWatch, WatchStatus, type Agent } from "@pulsebeam/react";

export function Roster({
  agent,
  roomName,
}: {
  agent: Agent;
  roomName: string;
}) {
  const room = useMemo(() => agent.room(roomName), [agent, roomName]);
  const [cursor, setCursor] = useState<readonly number[]>();
  const watch = useWatch(room, { limit: 100, cursor });
  const state = watch?.state();
  const first = () => setCursor(undefined);
  if (!state || state.status === WatchStatus.Loading)
    return <p>Loading roster…</p>;
  if (state.status === WatchStatus.Failed) {
    return (
      <section>
        <p role="alert">{state.error?.message ?? "Roster unavailable"}</p>
        {cursor && <button onClick={first}>First page</button>}
      </section>
    );
  }
  const page = state.page;
  if (!page) return <p>Roster closed</p>;
  return (
    <section>
      {page.participants.length === 0 && <p>No participants on this page.</p>}
      <ul>
        {page.participants.map((participant) => (
          <li key={participant.id}>
            {participant.id}
            {participant.publications.length === 0
              ? " · present, no publications"
              : ` · ${participant.publications.map((publication) => publication.trackId).join(", ")}`}
          </li>
        ))}
      </ul>
      <button onClick={first} disabled={!cursor}>
        First page
      </button>
      <button
        disabled={!page.hasMore}
        onClick={() => setCursor(page.nextCursor)}
      >
        Next page
      </button>
    </section>
  );
}
