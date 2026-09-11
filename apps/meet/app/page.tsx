"use client";

import { useCallback, useState } from "react";
import { Lobby } from "@/components/Lobby";
import { Room } from "@/components/Room";

export default function Home() {
  const [session, setSession] = useState<{
    token: string;
    endpoint: string;
    stream: MediaStream;
  } | null>(null);
  const leave = useCallback(() => setSession(null), []);
  return session ? (
    <Room {...session} onLeave={leave} />
  ) : (
    <Lobby
      onJoin={(token, endpoint, stream) =>
        setSession({ token, endpoint, stream })
      }
    />
  );
}
