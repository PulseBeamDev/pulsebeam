"use client";

import { useCallback, useState } from "react";
import { Lobby } from "./Lobby";
import { MeetMediaProvider, useMeetMedia } from "./MeetMediaProvider";
import { Room } from "./Room";

export function MeetApp() {
  return (
    <MeetMediaProvider>
      <MeetSession />
    </MeetMediaProvider>
  );
}

function MeetSession() {
  const [session, setSession] = useState<{
    token: string;
    endpoint: string;
    cameraOn: boolean;
    micOn: boolean;
  } | null>(null);
  const { capture } = useMeetMedia();
  const leave = useCallback(() => {
    capture.stop();
    setSession(null);
  }, [capture.stop]);
  return session ? (
    <Room {...session} onLeave={leave} />
  ) : (
    <Lobby
      onJoin={(token, endpoint, cameraOn, micOn) =>
        setSession({ token, endpoint, cameraOn, micOn })
      }
    />
  );
}
