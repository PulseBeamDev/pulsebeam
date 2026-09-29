import { useEffect, useState } from "react";
import { useMediaDevices } from "@pulsebeam/react";
import type { CaptureResult } from "@pulsebeam/react";
import { DeviceSelector } from "./DeviceSelector";
import { MediaPreview } from "./MediaPreview";
import { Button, Card, CardContent, Input } from "./ui";
import { defaultServerUrl } from "@/lib/config";
import { normalizeEndpoint } from "@/lib/model";
import { Radio, RefreshCw } from "lucide-react";

export function Lobby({
  capture,
  videoDeviceId,
  audioDeviceId,
  setVideoDeviceId,
  setAudioDeviceId,
  onJoin,
}: {
  capture: CaptureResult;
  videoDeviceId: string;
  audioDeviceId: string;
  setVideoDeviceId(id: string): void;
  setAudioDeviceId(id: string): void;
  onJoin(
    token: string,
    endpoint: string,
    cameraOn: boolean,
    micOn: boolean,
  ): void;
}) {
  const [token, setToken] = useState("");
  const [serverURL, setServerURL] = useState(defaultServerUrl);
  const devices = useMediaDevices();
  const { videoTrack, audioTrack, error, request } = capture;
  const [isMicOn, setMicOn] = useState(true);
  const [isCamOn, setCamOn] = useState(true);
  const endpoint = normalizeEndpoint(serverURL);
  useEffect(() => {
    void request().catch(() => {});
  }, [request]);
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
            videoTrack={videoTrack}
            isCamOn={isCamOn}
            isMicOn={isMicOn}
            onToggleCam={() => setCamOn((value) => !value)}
            onToggleMic={() => setMicOn((value) => !value)}
            hasStream={Boolean(videoTrack || audioTrack)}
          />
          {(error || devices.error) && (
            <div
              role="alert"
              className="flex items-center gap-3 rounded-lg border border-destructive/25 bg-destructive/10 p-3 text-sm text-destructive"
            >
              <span className="min-w-0 flex-1">
                {(error || devices.error)?.message}
              </span>
              <Button
                size="sm"
                variant="secondary"
                onClick={() => void request().catch(() => {})}
              >
                <RefreshCw className="size-3.5" /> Retry
              </Button>
            </div>
          )}
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <DeviceSelector
              label="Camera"
              value={videoDeviceId}
              devices={devices.cameras}
              onValueChange={setVideoDeviceId}
            />
            <DeviceSelector
              label="Microphone"
              value={audioDeviceId}
              devices={devices.microphones}
              onValueChange={setAudioDeviceId}
            />
          </div>
          <form
            onSubmit={(event) => {
              event.preventDefault();
              if (videoTrack && audioTrack && endpoint)
                onJoin(token, endpoint, isCamOn, isMicOn);
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
                value={serverURL}
                onChange={(event) => setServerURL(event.target.value)}
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
              disabled={!videoTrack || !audioTrack || !endpoint}
            >
              Join room
            </Button>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
