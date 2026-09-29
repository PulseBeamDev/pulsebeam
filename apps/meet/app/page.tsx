"use client";

import { useCallback, useState } from "react";
import { useUserMedia } from "@pulsebeam/react";
import { Lobby } from "@/components/Lobby";
import { Room } from "@/components/Room";

export default function Home() {
  const [session, setSession] = useState<{
    token: string;
    endpoint: string;
    cameraOn: boolean;
    micOn: boolean;
  } | null>(null);
  const [videoDeviceId, setVideoDeviceId] = useState("");
  const [audioDeviceId, setAudioDeviceId] = useState("");
  const capture = useUserMedia({
    video: {
      deviceId: videoDeviceId ? { exact: videoDeviceId } : undefined,
      width: { ideal: 1280 },
      height: { ideal: 720 },
      frameRate: { ideal: 30 },
    },
    audio: {
      deviceId: audioDeviceId ? { exact: audioDeviceId } : undefined,
      echoCancellation: true,
      noiseSuppression: true,
      autoGainControl: true,
    },
  });
  const leave = useCallback(() => {
    capture.stop();
    setSession(null);
  }, [capture.stop]);
  return session ? (
    <Room {...session} capture={capture} onLeave={leave} />
  ) : (
    <Lobby
      capture={capture}
      videoDeviceId={videoDeviceId}
      audioDeviceId={audioDeviceId}
      setVideoDeviceId={setVideoDeviceId}
      setAudioDeviceId={setAudioDeviceId}
      onJoin={(token, endpoint, cameraOn, micOn) =>
        setSession({ token, endpoint, cameraOn, micOn })
      }
    />
  );
}
