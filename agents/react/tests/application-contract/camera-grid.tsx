import { Video, useSubscribe, type RoomRef } from "@pulsebeam/react";

export function CameraGrid({ room }: { room: RoomRef }) {
  const cameras = useSubscribe(room.video({ trackId: "camera" }));
  return cameras?.state().tracks.map(({ publicationId, participantId, track }) => (
    <Video key={publicationId} source={track} aria-label={participantId} />
  ));
}
