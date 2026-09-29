import { Box, IconButton, Stack, Typography } from "@mui/material";
import { Mic, MicOff, Video as VideoIcon, VideoOff } from "lucide-react";
import { Video } from "@pulsebeam/react";
import type { CapturedVideoTrack } from "@pulsebeam/react";

export function MediaPreview({
  videoTrack,
  isCamOn,
  isMicOn,
  onToggleCam,
  onToggleMic,
  hasStream,
}: {
  videoTrack: CapturedVideoTrack | null;
  isCamOn: boolean;
  isMicOn: boolean;
  onToggleCam(): void;
  onToggleMic(): void;
  hasStream: boolean;
}) {
  const microphoneLabel = isMicOn ? "Mute microphone" : "Unmute microphone";
  const cameraLabel = isCamOn ? "Turn camera off" : "Turn camera on";
  return (
    <Box className="relative aspect-video overflow-hidden rounded-lg bg-slate-900">
      {hasStream && isCamOn ? (
        <Video
          source={videoTrack}
          autoPlay
          mirror
          className="h-full w-full object-contain"
        />
      ) : (
        <Stack className="h-full items-center justify-center text-slate-200">
          <VideoOff size={30} aria-hidden="true" />
          <Typography variant="body2">
            {!hasStream ? "Initializing camera…" : "Camera is off"}
          </Typography>
        </Stack>
      )}
      {hasStream && (
        <Stack
          direction="row"
          spacing={1}
          className="absolute bottom-3 left-1/2 -translate-x-1/2"
        >
          <IconButton
            title={microphoneLabel}
            aria-label={microphoneLabel}
            onClick={onToggleMic}
            sx={{
              bgcolor: isMicOn ? "#e4edf9" : "error.main",
              color: isMicOn ? "#12233d" : "white",
            }}
          >
            {isMicOn ? <Mic size={20} /> : <MicOff size={20} />}
          </IconButton>
          <IconButton
            title={cameraLabel}
            aria-label={cameraLabel}
            onClick={onToggleCam}
            sx={{
              bgcolor: isCamOn ? "#e4edf9" : "error.main",
              color: isCamOn ? "#12233d" : "white",
            }}
          >
            {isCamOn ? <VideoIcon size={20} /> : <VideoOff size={20} />}
          </IconButton>
        </Stack>
      )}
    </Box>
  );
}
