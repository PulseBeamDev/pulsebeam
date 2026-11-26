import { useMemo, useState } from "react";
import type { VideoCollectionEntry } from "@pulsebeam/react";

export function useVideoLayout(tracks: readonly VideoCollectionEntry[] = []) {
  const [pin, setPin] = useState<string | null>(null);
  const remoteTracks = useMemo(
    () =>
      [...tracks].sort(
        (a, b) =>
          a.participantId.localeCompare(b.participantId) ||
          a.trackId.localeCompare(b.trackId),
      ),
    [tracks],
  );
  const spotlight =
    pin === "local"
      ? null
      : (remoteTracks.find((entry) => entry.publicationId === pin) ??
        remoteTracks[0] ??
        null);
  return { remoteTracks, spotlight, setPin };
}
