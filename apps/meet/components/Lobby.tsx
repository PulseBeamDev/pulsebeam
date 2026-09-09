import { useEffect, useRef, useState } from "react";
import { DeviceSelector } from "./DeviceSelector";
import { MediaPreview } from "./MediaPreview";
import { Button, Card, CardContent, Input } from "./ui";
import { useMediaDevices } from "@/hooks/media";
import { normalizeEndpoint } from "@/lib/model";

export function Lobby({ onJoin }: { onJoin(roomId: string, endpoint: string, stream: MediaStream): void }) {
  const [roomId, setRoomId] = useState("");
  const [apiURL, setApiURL] = useState("http://localhost:7070/api/v1");
  const videoRef = useRef<HTMLVideoElement>(null);
  const media = useMediaDevices();
  const endpoint = normalizeEndpoint(apiURL);
  useEffect(() => { void media.startMedia(); }, [media.startMedia]);
  useEffect(() => { if (videoRef.current) videoRef.current.srcObject = media.stream; }, [media.stream]);
  return <div className="flex min-h-dvh items-center justify-center bg-background px-3 py-4"><Card className="w-full max-w-xl shadow-xl"><CardContent className="space-y-5 px-4 pt-4 sm:px-6">
    <MediaPreview videoRef={videoRef} isCamOn={media.isCamOn} isMicOn={media.isMicOn} onToggleCam={media.toggleVideo} onToggleMic={media.toggleAudio} hasStream={Boolean(media.stream)} />
    {media.error && <div role="alert" className="text-sm text-destructive">{media.error} <Button onClick={() => void media.startMedia()}>Retry</Button></div>}
    <div className="grid grid-cols-1 gap-4 sm:grid-cols-2"><DeviceSelector label="Camera" value={media.videoDeviceId} devices={media.devices.filter((d) => d.kind === "videoinput")} onValueChange={media.setVideoDeviceId} /><DeviceSelector label="Microphone" value={media.audioDeviceId} devices={media.devices.filter((d) => d.kind === "audioinput")} onValueChange={media.setAudioDeviceId} /></div>
    <form onSubmit={(event) => { event.preventDefault(); const stream = media.takeStream(); if (stream && endpoint) onJoin(roomId, endpoint, stream); }} className="space-y-3 border-t pt-4"><Input value={roomId} onChange={(event) => setRoomId(event.target.value)} placeholder="Room ID" required /><Input value={apiURL} onChange={(event) => setApiURL(event.target.value)} placeholder="API URL" inputMode="url" />{!endpoint && <p role="alert" className="text-sm text-destructive">Enter an absolute HTTP(S) URL without query or fragment.</p>}<Button type="submit" className="w-full" disabled={!media.stream || !endpoint}>Join Room</Button></form>
  </CardContent></Card></div>;
}
