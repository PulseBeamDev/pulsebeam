import { useEffect, useRef, useState } from "react";
import { DeviceSelector } from "./DeviceSelector";
import { MediaPreview } from "./MediaPreview";
import { Button, Card, CardContent, Input } from "./ui";
import { useMediaDevices } from "@/hooks/media";
import { defaultApiUrl } from "@/lib/config";
import { normalizeEndpoint } from "@/lib/model";

export function Lobby({
  onJoin,
}: {
  onJoin(roomId: string, endpoint: string, stream: MediaStream): void;
}) {
  const [roomId, setRoomId] = useState("");
  const [apiURL, setApiURL] = useState(defaultApiUrl);
  const videoRef = useRef<HTMLVideoElement>(null);
  const {
    stream,
    devices,
    videoDeviceId,
    audioDeviceId,
    error,
    isMicOn,
    isCamOn,
    startMedia,
    takeStream,
    toggleAudio,
    toggleVideo,
    setVideoDeviceId,
    setAudioDeviceId,
  } = useMediaDevices();
  const endpoint = normalizeEndpoint(apiURL);
  useEffect(() => {
    void startMedia();
  }, [startMedia]);
  useEffect(() => {
    if (videoRef.current) videoRef.current.srcObject = stream;
  }, [stream]);
  return (
    <div className="flex min-h-dvh items-center justify-center bg-background px-3 py-4">
      <Card className="w-full max-w-xl shadow-xl">
        <CardContent className="space-y-5 px-4 pt-4 sm:px-6">
          <MediaPreview
            videoRef={videoRef}
            isCamOn={isCamOn}
            isMicOn={isMicOn}
            onToggleCam={toggleVideo}
            onToggleMic={toggleAudio}
            hasStream={Boolean(stream)}
          />
          {error && (
            <div role="alert" className="text-sm text-destructive">
              {error} <Button onClick={() => void startMedia()}>Retry</Button>
            </div>
          )}
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <DeviceSelector
              label="Camera"
              value={videoDeviceId}
              devices={devices.filter((d) => d.kind === "videoinput")}
              onValueChange={setVideoDeviceId}
            />
            <DeviceSelector
              label="Microphone"
              value={audioDeviceId}
              devices={devices.filter((d) => d.kind === "audioinput")}
              onValueChange={setAudioDeviceId}
            />
          </div>
          <form
            onSubmit={(event) => {
              event.preventDefault();
              const activeStream = takeStream();
              if (activeStream && endpoint)
                onJoin(roomId, endpoint, activeStream);
            }}
            className="space-y-3 border-t pt-4"
          >
            <Input
              value={roomId}
              onChange={(event) => setRoomId(event.target.value)}
              placeholder="Room ID"
              required
            />
            <Input
              value={apiURL}
              onChange={(event) => setApiURL(event.target.value)}
              placeholder="API URL"
              inputMode="url"
            />
            {!endpoint && (
              <p role="alert" className="text-sm text-destructive">
                Enter an absolute HTTP(S) URL without query or fragment.
              </p>
            )}
            <Button
              type="submit"
              className="w-full"
              disabled={!stream || !endpoint}
            >
              Join Room
            </Button>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
