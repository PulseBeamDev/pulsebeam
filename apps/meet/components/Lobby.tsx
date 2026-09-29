import { useEffect, useState } from "react";
import {
  Alert,
  Box,
  Button,
  Paper,
  Stack,
  TextField,
  Typography,
} from "@mui/material";
import { DeviceSettings } from "./DeviceSettings";
import { MediaPreview } from "./MediaPreview";
import { useMeetMedia } from "./MeetMediaProvider";
import { defaultServerUrl } from "@/lib/config";
import { normalizeEndpoint } from "@/lib/model";
import { Radio, RefreshCw } from "lucide-react";

export function Lobby({
  onJoin,
}: {
  onJoin(
    token: string,
    endpoint: string,
    cameraOn: boolean,
    micOn: boolean,
  ): void;
}) {
  const [token, setToken] = useState("");
  const [serverURL, setServerURL] = useState(defaultServerUrl);
  const { capture, devices } = useMeetMedia();
  const { videoTrack, audioTrack, error, request } = capture;
  const [isMicOn, setMicOn] = useState(true);
  const [isCamOn, setCamOn] = useState(true);
  const endpoint = normalizeEndpoint(serverURL);
  useEffect(() => {
    void request().catch(() => {});
  }, [request]);
  return (
    <Box
      component="main"
      className="meet-lobby grid min-h-dvh place-items-center p-3 sm:p-6"
    >
      <Paper
        elevation={3}
        className="w-full max-w-[560px] p-4 sm:p-6"
        sx={{ borderRadius: 3 }}
      >
        <Stack spacing={2.5}>
          <Box>
            <Stack
              direction="row"
              spacing={1}
              sx={{ alignItems: "center", color: "primary.main" }}
            >
              <Radio size={22} />
              <Typography variant="subtitle1" sx={{ fontWeight: 700 }}>
                PulseBeam Meet
              </Typography>
            </Stack>
            <Typography variant="h5" component="h1" sx={{ fontWeight: 700 }}>
              Ready to join?
            </Typography>
            <Typography variant="body2" color="text.secondary">
              Check your camera and microphone before entering.
            </Typography>
          </Box>
          <MediaPreview
            videoTrack={videoTrack}
            isCamOn={isCamOn}
            isMicOn={isMicOn}
            onToggleCam={() => setCamOn((value) => !value)}
            onToggleMic={() => setMicOn((value) => !value)}
            hasStream={Boolean(videoTrack || audioTrack)}
          />
          {(error || devices.error) && (
            <Alert
              severity="error"
              action={
                <Button
                  startIcon={<RefreshCw size={16} />}
                  onClick={() => void request().catch(() => {})}
                >
                  Retry
                </Button>
              }
            >
              {(error || devices.error)?.message}
            </Alert>
          )}
          <Stack direction={{ xs: "column", sm: "row" }} spacing={2}>
            <DeviceSettings />
          </Stack>
          <Stack
            component="form"
            spacing={2}
            onSubmit={(event) => {
              event.preventDefault();
              if (videoTrack && audioTrack && endpoint)
                onJoin(token, endpoint, isCamOn, isMicOn);
            }}
          >
            <TextField
              label="Bearer token"
              type="password"
              fullWidth
              required
              value={token}
              onChange={(event) => setToken(event.target.value)}
              autoComplete="off"
            />
            <TextField
              label="Server URL"
              fullWidth
              value={serverURL}
              inputMode="url"
              onChange={(event) => setServerURL(event.target.value)}
              error={!endpoint}
              helperText={
                !endpoint &&
                "Enter an absolute HTTP(S) URL without query or fragment."
              }
            />
            <Button
              type="submit"
              size="large"
              variant="contained"
              fullWidth
              disabled={!videoTrack || !audioTrack || !endpoint}
            >
              Join room
            </Button>
          </Stack>
        </Stack>
      </Paper>
    </Box>
  );
}
