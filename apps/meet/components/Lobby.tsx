import { useEffect, useRef, useState } from "react";
import { DeviceSelector } from "./DeviceSelector";
import { MediaPreview } from "./MediaPreview";
import { Button, Card, CardContent, Input } from "./ui";
import { useMediaDevices } from "@/hooks/media";
import { defaultApiUrl } from "@/lib/config";
import { normalizeEndpoint } from "@/lib/model";
import { Radio, RefreshCw } from "lucide-react";

export function Lobby({
  onJoin,
}: {
  onJoin(token: string, endpoint: string, stream: MediaStream): void;
}) {
  const [token, setToken] = useState("");
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
    <div className="relative flex min-h-dvh items-center justify-center overflow-hidden bg-background px-3 py-4 sm:p-6">
      <div className="pointer-events-none absolute inset-0 bg-[radial-gradient(circle_at_50%_-10%,color-mix(in_oklab,var(--primary)_20%,transparent),transparent_42%)]" />
      <Card className="relative w-full max-w-xl shadow-xl sm:shadow-2xl">
        <CardContent className="space-y-5 px-4 pt-4 sm:space-y-6 sm:px-6 sm:pt-6">
          <header className="flex items-center justify-between gap-4">
            <div>
              <div className="mb-1 flex items-center gap-2 text-primary">
                <span className="grid size-8 place-items-center rounded-lg bg-primary text-primary-foreground shadow-sm shadow-primary/25">
                  <Radio className="size-4" />
                </span>
                <span className="text-sm font-semibold tracking-tight text-foreground">
                  PulseBeam Meet
                </span>
              </div>
              <h1 className="text-xl font-semibold tracking-tight sm:text-2xl">
                Ready to join?
              </h1>
              <p className="mt-1 text-sm text-muted-foreground">
                Check your camera and microphone before entering.
              </p>
            </div>
          </header>
          <MediaPreview
            videoRef={videoRef}
            isCamOn={isCamOn}
            isMicOn={isMicOn}
            onToggleCam={toggleVideo}
            onToggleMic={toggleAudio}
            hasStream={Boolean(stream)}
          />
          {error && (
            <div
              role="alert"
              className="flex items-center gap-3 rounded-lg border border-destructive/25 bg-destructive/10 p-3 text-sm text-destructive"
            >
              <span className="min-w-0 flex-1">{error}</span>
              <Button
                size="sm"
                variant="secondary"
                onClick={() => void startMedia()}
              >
                <RefreshCw className="size-3.5" /> Retry
              </Button>
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
                onJoin(token, endpoint, activeStream);
            }}
            className="space-y-3 border-t pt-4"
          >
            <label className="block space-y-2">
              <span className="text-xs font-semibold tracking-wider text-muted-foreground uppercase">
                Token
              </span>
              <Input
                className="h-11 sm:h-9"
                type="password"
                value={token}
                onChange={(event) => setToken(event.target.value)}
                placeholder="Enter a bearer token"
                autoComplete="off"
                autoCapitalize="none"
                spellCheck={false}
                required
              />
            </label>
            <label className="block space-y-2">
              <span className="text-xs font-semibold tracking-wider text-muted-foreground uppercase">
                Server
              </span>
              <Input
                className="h-11 sm:h-9"
                value={apiURL}
                onChange={(event) => setApiURL(event.target.value)}
                placeholder="https://demo.pulsebeam.dev"
                inputMode="url"
                spellCheck={false}
              />
            </label>
            {!endpoint && (
              <p role="alert" className="text-sm text-destructive">
                Enter an absolute HTTP(S) URL without query or fragment.
              </p>
            )}
            <Button
              type="submit"
              className="h-11 w-full sm:h-9"
              disabled={!stream || !endpoint}
            >
              Join room
            </Button>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
