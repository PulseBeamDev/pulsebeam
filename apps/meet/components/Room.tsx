import { useCallback, useEffect, useState } from "react";
import { Audio, Video, useAgent } from "@pulsebeam/react";
import type { Agent, RemoteVideoTrack } from "@pulsebeam/react";
import {
  Alert,
  Box,
  Button,
  Chip,
  IconButton,
  Menu,
  MenuItem,
  Paper,
  Popover,
  Stack,
  TextField,
  Typography,
} from "@mui/material";
import * as Icons from "lucide-react";
import { useRoomMedia } from "@/hooks/room-media";
import { DeviceSettings } from "./DeviceSettings";
import { useMeetMedia } from "./MeetMediaProvider";
import { useTopics } from "@/hooks/topics";
import { useVideoLayout } from "@/hooks/video-layout";

const latencyModes: Record<
  string,
  { minMs: number; maxMs: number } | undefined
> = {
  Auto: undefined,
  Smooth: { minMs: 400, maxMs: 800 },
  Balanced: { minMs: 100, maxMs: 200 },
  Zero: { minMs: 0, maxMs: 0 },
};
const reactionEmojis = ["👍", "❤️", "😂", "😮", "👏", "🔥"];

export function Room({
  token,
  endpoint,
  cameraOn,
  micOn,
  onLeave,
}: {
  token: string;
  endpoint: string;
  cameraOn: boolean;
  micOn: boolean;
  onLeave(): void;
}) {
  const agent = useAgent({
    endpoint,
    token,
    topology: {
      localVideos: 2,
      localAudios: 1,
      remoteVideos: 7,
      remoteAudios: 3,
    },
    logging: { level: "debug" },
  });
  return agent ? (
    <RoomSession
      agent={agent}
      initial={{ cameraOn, micOn }}
      onLeave={onLeave}
    />
  ) : (
    <Box component="main" className="grid h-dvh place-items-center">
      Joining room…
    </Box>
  );
}

function RoomSession({
  agent: owner,
  initial,
  onLeave,
}: {
  agent: Agent;
  initial: { cameraOn: boolean; micOn: boolean };
  onLeave(): void;
}) {
  const agent = owner.getSnapshot();
  const { capture, devices } = useMeetMedia();
  const [deviceAnchor, setDeviceAnchor] = useState<HTMLElement | null>(null);
  const [reactionAnchor, setReactionAnchor] = useState<HTMLElement | null>(
    null,
  );
  const [latencyMode, setLatencyMode] = useState("Auto");
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [chatOpen, setChatOpen] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [playbackRetry, setPlaybackRetry] = useState<
    (() => Promise<void>) | null
  >(null);
  const { screen, camera, cameraOn, micOn, detachScreen, startShare, toggle } =
    useRoomMedia(
      owner,
      { videoTrack: capture.videoTrack, audioTrack: capture.audioTrack },
      setFailure,
      initial,
    );
  const {
    messages,
    reactions,
    sendChat,
    sendReaction,
    error: topicError,
    clearError,
    gap,
    clearGap,
    retrySubscriptions,
  } = useTopics(owner, agent.participantExternalId);
  const { remoteTracks, spotlight, setPin } = useVideoLayout(
    owner.remoteVideoTracks,
  );
  useEffect(() => {
    owner.connect();
  }, [owner]);
  useEffect(() => {
    for (const track of remoteTracks)
      track.setReceiveOptions({
        minFps: 15,
        priority: track === spotlight ? 200 : 10,
        playoutDelay: latencyModes[latencyMode],
      });
  }, [latencyMode, remoteTracks, spotlight]);
  const playbackError = useCallback(
    ({ error, retry }: { error: unknown; retry: () => Promise<void> }) => {
      setFailure(
        `Playback: ${error instanceof Error ? error.message : String(error)}`,
      );
      setPlaybackRetry(() => retry);
    },
    [],
  );
  const closeAlert = () => {
    setFailure(null);
    setPlaybackRetry(null);
    clearError();
  };
  const send = async () => {
    if (sending || !draft.trim()) return;
    const text = draft;
    setSending(true);
    try {
      if (await sendChat(text))
        setDraft((current) => (current === text ? "" : current));
    } finally {
      setSending(false);
    }
  };
  const selectReaction = (emoji: string) => {
    sendReaction(emoji);
    setReactionAnchor(null);
  };
  const video = (track: RemoteVideoTrack | null, fill: "contain" | "cover") => (
    <Video
      source={track ?? camera}
      autoPlay
      mirror={!track}
      className={
        fill === "cover"
          ? "h-full w-full object-cover"
          : "h-full w-full object-contain"
      }
      onPlaybackError={playbackError}
    />
  );
  const tile = (track: RemoteVideoTrack | null) => (
    <Box
      component="button"
      key={track ? `${track.participantId}:${track.label}` : "local"}
      aria-label={`Spotlight ${track?.participantId ?? "your camera"}`}
      onClick={() => setPin(track ?? "local")}
      className="relative aspect-video w-36 shrink-0 cursor-pointer overflow-hidden rounded-lg border-2 border-transparent bg-slate-900 p-0 hover:border-blue-500 focus-visible:border-blue-500 lg:w-full"
    >
      {video(track, "cover")}
      <Typography
        variant="caption"
        className="absolute bottom-1 left-1.5 max-w-[90%] truncate rounded bg-slate-950/80 px-1.5 text-white!"
      >
        {track?.participantId ?? "You"}
      </Typography>
    </Box>
  );
  return (
    <Box className="flex h-dvh min-w-0 flex-col bg-slate-50">
      <Paper
        component="header"
        square
        elevation={0}
        className="flex shrink-0 items-center justify-between gap-2 border-b border-slate-200 px-2 py-2 sm:px-4 max-sm:[&_.MuiIconButton-root]:p-[5px]!"
      >
        <Stack
          direction="row"
          spacing={1}
          sx={{ minWidth: 0, alignItems: "center" }}
        >
          <Chip
            size="small"
            color={agent.connection === "connected" ? "success" : "warning"}
            label={agent.participantExternalId ?? "Joining…"}
            className="max-w-24 sm:max-w-65"
          />
          <Typography
            variant="caption"
            color="text.secondary"
            sx={{ display: { xs: "none", sm: "block" } }}
          >
            {agent.roomExternalId ? `Room ${agent.roomExternalId} · ` : ""}
            {agent.connection}
          </Typography>
        </Stack>
        <Stack
          direction="row"
          spacing={0.25}
          sx={{ flexShrink: 0, alignItems: "center" }}
        >
          <IconButton
            aria-label={screen ? "Stop sharing" : "Share screen"}
            onClick={() => (screen ? detachScreen() : void startShare())}
          >
            {screen ? (
              <Icons.MonitorOff size={20} />
            ) : (
              <Icons.Monitor size={20} />
            )}
          </IconButton>
          <IconButton
            aria-label="Chat"
            aria-pressed={chatOpen}
            color={chatOpen ? "primary" : "default"}
            onClick={() => setChatOpen(!chatOpen)}
          >
            <Icons.MessageCircle size={20} />
          </IconButton>
          <IconButton
            aria-label="Devices and latency"
            onClick={(event) => setDeviceAnchor(event.currentTarget)}
          >
            <Icons.Settings2 size={20} />
          </IconButton>
          <IconButton aria-label="Reconnect" onClick={() => owner.reconnect()}>
            <Icons.RotateCcw size={20} />
          </IconButton>
          <Button
            color="error"
            variant="contained"
            onClick={onLeave}
            startIcon={<Icons.PhoneOff size={17} />}
            sx={{ minWidth: 72 }}
          >
            Leave
          </Button>
        </Stack>
      </Paper>
      <Popover
        open={Boolean(deviceAnchor)}
        anchorEl={deviceAnchor}
        onClose={() => setDeviceAnchor(null)}
        anchorOrigin={{ vertical: "bottom", horizontal: "right" }}
        transformOrigin={{ vertical: "top", horizontal: "right" }}
      >
        <Stack spacing={2} sx={{ p: 2, width: "min(340px, 95vw)" }}>
          <Typography variant="h6">Call settings</Typography>
          <DeviceSettings />
          <TextField
            select
            size="small"
            fullWidth
            label="Latency"
            value={latencyMode}
            onChange={(event) => setLatencyMode(event.target.value)}
          >
            {Object.keys(latencyModes).map((mode) => (
              <MenuItem key={mode} value={mode}>
                {mode}
              </MenuItem>
            ))}
          </TextField>
        </Stack>
      </Popover>
      {(capture.error || devices.error) && (
        <Alert
          severity="error"
          action={
            <Button onClick={() => void capture.request().catch(() => {})}>
              Retry capture
            </Button>
          }
        >
          {(capture.error || devices.error)?.message}
        </Alert>
      )}
      {(failure || topicError || agent.failure) && (
        <Alert
          severity="error"
          onClose={closeAlert}
          action={
            <Stack direction="row">
              {playbackRetry && (
                <Button onClick={() => void playbackRetry()}>
                  Retry playback
                </Button>
              )}
              {topicError && (
                <Button onClick={retrySubscriptions}>Retry chat</Button>
              )}
            </Stack>
          }
        >
          {failure ?? topicError ?? agent.failure?.message}
        </Alert>
      )}
      {gap && (
        <Alert severity="warning" onClose={clearGap}>
          Some chat messages may be missing after recovery.
        </Alert>
      )}
      <Box
        component="main"
        className="relative flex min-h-0 min-w-0 flex-1 flex-col gap-3 p-2 lg:flex-row lg:p-4"
      >
        <Paper
          className="relative grid min-h-0 min-w-0 flex-1 place-items-center overflow-hidden"
          sx={{ bgcolor: "#0b1220" }}
        >
          <Box sx={{ width: "100%", height: "100%", position: "relative" }}>
            {video(spotlight, "contain")}
            {spotlight && (
              <Chip
                size="small"
                label={spotlight.participantId}
                className="absolute! top-3 left-3 max-w-[80%]"
                sx={{ bgcolor: "#15233a", color: "white" }}
              />
            )}
            {reactions.map((reaction, index) => (
              <span
                key={reaction.id}
                aria-label={reaction.emoji}
                className="meet-reaction"
                style={{ left: `${24 + (index % 5) * 13}%` }}
              >
                {reaction.emoji}
              </span>
            ))}
            <Stack
              direction="row"
              spacing={1}
              className="absolute bottom-4 left-1/2 -translate-x-1/2 rounded-lg bg-slate-900/90 p-1.5"
            >
              <IconButton
                aria-label={micOn ? "Mute microphone" : "Unmute microphone"}
                onClick={() => toggle("microphone", !micOn)}
                className={
                  micOn
                    ? "bg-slate-700! text-white!"
                    : "bg-red-600! text-white!"
                }
              >
                {micOn ? <Icons.Mic size={22} /> : <Icons.MicOff size={22} />}
              </IconButton>
              <IconButton
                aria-label={cameraOn ? "Turn camera off" : "Turn camera on"}
                onClick={() => toggle("camera", !cameraOn)}
                className={
                  cameraOn
                    ? "bg-slate-700! text-white!"
                    : "bg-red-600! text-white!"
                }
              >
                {cameraOn ? (
                  <Icons.Video size={22} />
                ) : (
                  <Icons.VideoOff size={22} />
                )}
              </IconButton>
              <IconButton
                title="Send a reaction"
                aria-label="Send a reaction"
                aria-expanded={Boolean(reactionAnchor)}
                onClick={(event) => setReactionAnchor(event.currentTarget)}
                sx={{ bgcolor: "#34465e", color: "white" }}
              >
                <Icons.SmilePlus size={22} />
              </IconButton>
            </Stack>
          </Box>
        </Paper>
        <Menu
          open={Boolean(reactionAnchor)}
          anchorEl={reactionAnchor}
          onClose={() => setReactionAnchor(null)}
        >
          {reactionEmojis.map((emoji) => (
            <MenuItem
              key={emoji}
              aria-label={`React with ${emoji}`}
              onClick={() => selectReaction(emoji)}
            >
              {emoji}
            </MenuItem>
          ))}
        </Menu>
        <Stack
          component="aside"
          spacing={1}
          className="h-[110px] min-h-0 w-full shrink-0 lg:h-full lg:w-52"
        >
          <Typography variant="overline" color="text.secondary">
            Participants · {remoteTracks.length + 1}
          </Typography>
          <Stack
            direction={{ xs: "row", lg: "column" }}
            spacing={1}
            sx={{ overflow: "auto", flex: 1, minHeight: 0 }}
          >
            {spotlight && tile(null)}
            {remoteTracks
              .filter((track) => track !== spotlight)
              .map((track) => tile(track))}
          </Stack>
        </Stack>
        {chatOpen && (
          <Paper
            component="aside"
            className="absolute inset-x-0 bottom-0 z-10 flex h-[min(65dvh,480px)] min-h-0 w-full shrink-0 flex-col lg:static lg:h-auto lg:w-80"
          >
            <Stack
              direction="row"
              className="items-center justify-between border-b border-slate-200 px-4 py-2"
            >
              <Typography variant="subtitle1">Chat</Typography>
              <IconButton
                aria-label="Close chat"
                onClick={() => setChatOpen(false)}
              >
                <Icons.X size={18} />
              </IconButton>
            </Stack>
            <Stack spacing={1.5} className="min-h-0 flex-1 overflow-y-auto p-4">
              {!messages.length && (
                <Typography variant="body2" color="text.secondary">
                  No messages yet
                </Typography>
              )}
              {messages.map((message) => (
                <Box
                  key={message.id}
                  className={`max-w-[90%] ${message.self ? "self-end" : "self-start"}`}
                >
                  <Typography variant="caption" color="text.secondary">
                    {message.self ? "You" : message.sender}
                    {message.status === "pending" ? " · Sending…" : ""}
                  </Typography>
                  <Paper
                    elevation={0}
                    className={`break-words px-3 py-2 ${message.self ? "bg-blue-700! text-white!" : "bg-slate-100!"}`}
                  >
                    {message.text}
                  </Paper>
                </Box>
              ))}
            </Stack>
            <Stack
              component="form"
              direction="row"
              spacing={1}
              className="border-t border-slate-200 p-3"
              onSubmit={(event) => {
                event.preventDefault();
                void send();
              }}
            >
              <TextField
                size="small"
                fullWidth
                label="Message"
                value={draft}
                onChange={(event) => setDraft(event.target.value)}
              />
              <IconButton
                type="submit"
                color="primary"
                aria-label="Send message"
                disabled={
                  !draft.trim() || sending || !agent.participantExternalId
                }
              >
                <Icons.Send size={20} />
              </IconButton>
            </Stack>
          </Paper>
        )}
      </Box>
      <Audio source={owner.remoteAudio} onPlaybackError={playbackError} />
    </Box>
  );
}
