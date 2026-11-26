import { useState } from "react";
import { WatchStatus, useWatch, type RoomRef } from "@pulsebeam/react";

export function Roster({ room }: { room: RoomRef }) {
  const [cursor, setCursor] = useState<Uint8Array>();
  const roster = useWatch(room, { limit: 2, cursor });
  const page = roster?.state();
  if (!page || page.status === WatchStatus.Loading)
    return <p>Loading roster</p>;
  if (page.status !== WatchStatus.Live) return <p>Roster unavailable</p>;

  return (
    <>
      <ul>
        {page.participants.map((participant) => (
          <li key={participant.id()}>{participant.id()}</li>
        ))}
      </ul>
      <button type="button" onClick={() => setCursor(undefined)}>
        First page
      </button>
      <button
        type="button"
        disabled={!page.hasMore}
        onClick={() => setCursor(page.nextCursor)}
      >
        Next page
      </button>
    </>
  );
}
