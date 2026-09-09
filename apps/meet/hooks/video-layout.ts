import { useEffect, useMemo, useRef, useState } from "react";
import type { Publication } from "@pulsebeam/react";
import { allocateVideo, type VideoSelection } from "@/lib/model";

export function useVideoLayout(
  publications: readonly Publication[],
  participantId: string | null,
) {
  const [pin, setPin] = useState<string | "local" | null>(null);
  const [selection, setSelection] = useState<VideoSelection[]>([]);
  const [thumbnailHeights, setThumbnailHeights] = useState<
    Record<string, number>
  >({});
  const [spotlightHeight, setSpotlightHeight] = useState(0);
  const tiles = useRef(new Map<string, HTMLButtonElement>());
  const spotlightFrame = useRef<HTMLDivElement>(null);

  const remotePublications = useMemo(
    () =>
      publications
        .filter(
          (publication) =>
            publication.kind === "video" &&
            publication.participantId !== participantId,
        )
        .sort((a, b) => a.id.localeCompare(b.id)),
    [participantId, publications],
  );
  const publicationById = useMemo(
    () =>
      new Map(
        remotePublications.map((publication) => [publication.id, publication]),
      ),
    [remotePublications],
  );
  const remoteIds = useMemo(
    () => remotePublications.map((publication) => publication.id),
    [remotePublications],
  );
  const spotlight =
    pin === "local"
      ? null
      : pin && publicationById.has(pin)
        ? pin
        : (remoteIds[0] ?? null);
  const selected = useMemo(
    () =>
      allocateVideo(
        remoteIds,
        spotlight,
        selection,
        thumbnailHeights,
        spotlightHeight,
      ),
    [remoteIds, selection, spotlight, spotlightHeight, thumbnailHeights],
  );

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- retained slots are external desired-state identity.
    setSelection((previous) =>
      previous.length === selected.length &&
      previous.every(
        (entry, index) =>
          entry.id === selected[index].id &&
          entry.slot === selected[index].slot &&
          entry.height === selected[index].height,
      )
        ? previous
        : selected,
    );
    if (pin && pin !== "local" && !publicationById.has(pin)) setPin(null);
  }, [pin, publicationById, selected]);

  useEffect(() => {
    const observer = new ResizeObserver((entries) =>
      setThumbnailHeights((old) => {
        const next = { ...old };
        for (const entry of entries) {
          const id = (entry.target as HTMLElement).dataset.publication;
          if (id) next[id] = entry.contentRect.height;
        }
        return next;
      }),
    );
    for (const element of tiles.current.values()) observer.observe(element);
    return () => observer.disconnect();
  }, [remoteIds, spotlight]);

  useEffect(() => {
    const element = spotlightFrame.current;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) =>
      setSpotlightHeight(entry?.contentRect.height ?? 0),
    );
    observer.observe(element);
    return () => observer.disconnect();
  }, [spotlight]);

  return {
    remotePublications,
    publicationById,
    spotlight,
    selected,
    tiles,
    spotlightFrame,
    setPin,
  };
}
