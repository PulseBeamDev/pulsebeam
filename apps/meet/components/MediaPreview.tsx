import { Box, IconButton, Stack, Typography } from "@mui/material";
import { Mic, MicOff, Video as VideoIcon, VideoOff } from "lucide-react";
import { Video } from "@pulsebeam/react";
import { useMeetMedia } from "./MeetMediaProvider";

export function MediaPreview() {
  const { capture, cameraOn, micOn, setCameraOn, setMicOn } = useMeetMedia();
  const hasStream = Boolean(capture.video || capture.audio);
  const microphoneLabel = micOn ? "Mute microphone" : "Unmute microphone";
  const cameraLabel = cameraOn ? "Turn camera off" : "Turn camera on";
  return (
    <Box className="relative aspect-video overflow-hidden rounded-lg bg-slate-900">
      {hasStream && cameraOn ? (
        <Video
          source={capture.video}
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
            onClick={() => setMicOn((value) => !value)}
            sx={{
              bgcolor: micOn ? "#e4edf9" : "error.main",
              color: micOn ? "#12233d" : "white",
            }}
          >
            {micOn ? <Mic size={20} /> : <MicOff size={20} />}
          </IconButton>
          <IconButton
            title={cameraLabel}
            aria-label={cameraLabel}
            onClick={() => setCameraOn((value) => !value)}
            sx={{
              bgcolor: cameraOn ? "#e4edf9" : "error.main",
              color: cameraOn ? "#12233d" : "white",
            }}
          >
            {cameraOn ? <VideoIcon size={20} /> : <VideoOff size={20} />}
          </IconButton>
        </Stack>
      )}
    </Box>
  );
}
