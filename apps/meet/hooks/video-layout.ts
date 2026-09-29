import { useMemo, useState } from "react";
import type { RemoteVideoTrack } from "@pulsebeam/react";

export function useVideoLayout(tracks: readonly RemoteVideoTrack[]) {
  const [pin, setPin] = useState<RemoteVideoTrack | "local" | null>(null);
  const remoteTracks = useMemo(
    () =>
      [...tracks].sort(
        (a, b) =>
          a.participantId.localeCompare(b.participantId) ||
          a.label.localeCompare(b.label),
      ),
    [tracks],
  );
  const spotlight =
    pin === "local"
      ? null
      : pin && remoteTracks.includes(pin)
        ? pin
        : (remoteTracks[0] ?? null);
  return { remoteTracks, spotlight, setPin };
}
