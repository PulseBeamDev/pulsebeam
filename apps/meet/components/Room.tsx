import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Audio,
  Data,
  usePublish,
  useSubscribe,
  useWatch,
  WatchStatus,
  CatalogDelivery,
  Video,
  useAgent,
  useCapture,
  type Agent,
  type VideoProps,
  type VideoObservation,
  type RemoteAudioTrack,
  type RemoteDataTrack,
  type RemoteVideoTrack,
} from "@pulsebeam/react";
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
import { DeviceSettings } from "./DeviceSettings";
import { useMeetMedia } from "./MeetMediaProvider";
import { useChat } from "@/hooks/chat";
import { reactionEmojis, useReaction } from "@/hooks/reaction";
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

function RoomVideo({
  track,
  local,
  preferences,
  onPlaybackError,
  className,
}: {
  track: RemoteVideoTrack | null;
  local: VideoProps["source"];
  preferences?: VideoObservation;
  onPlaybackError: VideoProps["onPlaybackError"];
  className: string;
}) {
  const subscription = useSubscribe(track, preferences);
  return (
    <Video
      source={track ? subscription : local}
      autoPlay
      mirror={!track}
      className={className}
      onPlaybackError={onPlaybackError}
    />
  );
}

function RoomAudio({
  track,
  onPlaybackError,
}: {
  track: RemoteAudioTrack;
  onPlaybackError: VideoProps["onPlaybackError"];
}) {
  const subscription = useSubscribe(track);
  return (
    <Audio source={subscription} autoPlay onPlaybackError={onPlaybackError} />
  );
}

export function Room(props: {
  token: string;
  endpoint: string;
  room: string;
  onLeave(): void;
}) {
  const agent = useAgent({
    endpoint: props.endpoint,
    token: props.token,
    logging: { level: "debug" },
  });
  const displayCapture = useCapture("display", {
    video: { width: 1920, height: 1080, frameRate: 30 },
    audio: false,
  });
  if (!agent) return <Typography role="status">Preparing session…</Typography>;
  return (
    <ActiveRoom {...props} agent={agent} displayCapture={displayCapture} />
  );
}

function ActiveRoom({
  token,
  endpoint,
  room,
  onLeave,
  agent,
  displayCapture,
}: {
  token: string;
  endpoint: string;
  room: string;
  onLeave(): void;
  agent: Agent;
  displayCapture: ReturnType<typeof useCapture>;
}) {
  const { capture, devices, cameraOn, micOn, setCameraOn, setMicOn } =
    useMeetMedia();
  const {
    video: screen,
    stop: stopShare,
    request: requestShare,
  } = displayCapture;
  const Camera = agent.video("camera");
  const Microphone = agent.audio("microphone");
  const Screen = agent.video("screen");
  const [deviceAnchor, setDeviceAnchor] = useState<HTMLElement | null>(null);
  const [reactionAnchor, setReactionAnchor] = useState<HTMLElement | null>(
    null,
  );
  const [latencyMode, setLatencyMode] = useState("Auto");
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [chatOpen, setChatOpen] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const playbackRetry = useRef<(() => Promise<void>) | null>(null);
  const joinedRoom = useRef<string | null>(null);
  const [catalogScope, setCatalogScope] = useState({
    room,
    cursor: [] as readonly number[],
    retry: 0,
  });
  const pinnedRoom = useMemo(
    () => agent.room(room),
    [agent, room, catalogScope.retry],
  );
  const cursor = catalogScope.room === room ? catalogScope.cursor : [];
  const watch = useWatch(pinnedRoom, { limit: 100, cursor });
  const catalogState = watch?.state();
  const page =
    catalogState?.status === WatchStatus.Live ? catalogState.page : undefined;
  const firstPage = () =>
    setCatalogScope((scope) => ({ room, cursor: [], retry: scope.retry + 1 }));
  usePublish(Camera, capture.video, { enabled: cameraOn });
  usePublish(Microphone, capture.audio, { enabled: micOn });
  const screenPublication = usePublish(Screen, screen);
  useEffect(() => {
    if (!screenPublication) return;
    void screenPublication
      .configureSender({ contentHint: "detail" })
      .catch((error: unknown) => {
        setFailure(
          `Screen share: ${error instanceof Error ? error.message : String(error)}`,
        );
      });
  }, [screenPublication]);
  const {
    messages,
    error: chatError,
    clearError: clearChatError,
    onEvent: receiveChat,
    onError: onChatError,
    send: sendChat,
  } = useChat(agent);
  const {
    reactions,
    onEvent: receiveReaction,
    dismiss: dismissReaction,
    send: sendReaction,
  } = useReaction(agent);
  const {
    remoteVideoTracks,
    remoteAudioTracks,
    remoteChatTracks,
    remoteReactionTracks,
  } = useMemo(() => {
    const videos: RemoteVideoTrack[] = [];
    const audios: RemoteAudioTrack[] = [];
    const chats: RemoteDataTrack<"ordered">[] = [];
    const reactions: RemoteDataTrack<"lossy">[] = [];
    if (agent.roomId !== room)
      return {
        remoteVideoTracks: videos,
        remoteAudioTracks: audios,
        remoteChatTracks: chats,
        remoteReactionTracks: reactions,
      };
    for (const participant of page?.participants ?? []) {
      if (participant.id === agent.participantId) continue;
      for (const track of participant.publications) {
        if (track.kind.tag === "Video")
          videos.push(
            pinnedRoom.participant(participant.id).video(track.trackId),
          );
        if (track.kind.tag === "Audio")
          audios.push(
            pinnedRoom.participant(participant.id).audio(track.trackId),
          );
        if (track.kind.tag !== "Data") continue;
        if (
          track.trackId === "chat" &&
          track.kind.inner.delivery === CatalogDelivery.Ordered
        )
          chats.push(
            pinnedRoom.participant(participant.id).data("chat", "ordered"),
          );
        if (
          track.trackId === "reactions" &&
          track.kind.inner.delivery === CatalogDelivery.Lossy
        )
          reactions.push(
            pinnedRoom.participant(participant.id).data("reactions", "lossy"),
          );
      }
    }
    return {
      remoteVideoTracks: videos,
      remoteAudioTracks: audios,
      remoteChatTracks: chats,
      remoteReactionTracks: reactions,
    };
  }, [agent, room, agent.roomId, agent.participantId, pinnedRoom, page]);
  const { remoteTracks, spotlight, setPin } = useVideoLayout(remoteVideoTracks);
  const connect = useCallback(() => {
    try {
      agent.join(room);
      joinedRoom.current = room;
    } catch (reason) {
      setFailure(
        `Connection: ${reason instanceof Error ? reason.message : String(reason)}`,
      );
    }
  }, [agent, room]);
  useEffect(() => {
    if (joinedRoom.current === room) return;
    connect();
  }, [connect, room]);
  const startShare = async () => {
    try {
      await requestShare();
    } catch (reason) {
      setFailure(
        reason instanceof Error
          ? `Screen share: ${reason.message}`
          : "Screen sharing was cancelled",
      );
    }
  };
  const playbackError = useCallback(
    ({ error, retry }: { error: unknown; retry: () => Promise<void> }) => {
      setFailure(
        `Playback: ${error instanceof Error ? error.message : String(error)}`,
      );
      playbackRetry.current = retry;
    },
    [],
  );
  const recoverPlayback = () => {
    const retry = playbackRetry.current;
    playbackRetry.current = null;
    if (retry)
      void retry().catch((error: unknown) => playbackError({ error, retry }));
  };
  const closeAlert = () => {
    setFailure(null);
    playbackRetry.current = null;
    clearChatError();
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
    <RoomVideo
      track={track}
      local={cameraOn ? capture.video : null}
      preferences={
        track
          ? {
              visible: true,
              height: 0,
              minHeight: 0,
              minFps: 15,
              priority: track === spotlight ? 200 : 10,
              playoutDelay: latencyModes[latencyMode]
                ? { mode: "fixed", ...latencyModes[latencyMode]! }
                : { mode: "adaptive" },
            }
          : undefined
      }
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
      key={track ? `${track.participantId}:${track.trackId}` : "local"}
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
    <Box
      className="flex h-dvh min-w-0 flex-col bg-slate-50"
      onClickCapture={recoverPlayback}
      onKeyDownCapture={recoverPlayback}
    >
      {remoteAudioTracks.map((track) => (
        <RoomAudio
          key={`${room}:${track.participantId}:${track.trackId}`}
          track={track}
          onPlaybackError={playbackError}
        />
      ))}
      {remoteChatTracks.map((track) => (
        <Data
          key={`${track.room}:${track.participantId}:${track.trackId}`}
          track={track}
          onEvent={receiveChat}
          onError={onChatError}
        />
      ))}
      {remoteReactionTracks.map((track) => (
        <Data
          key={`${track.room}:${track.participantId}:${track.trackId}`}
          track={track}
          onEvent={receiveReaction}
        />
      ))}
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
            label={agent.participantId ?? "Joining…"}
            className="max-w-24 sm:max-w-65"
          />
          <Typography
            variant="caption"
            color="text.secondary"
            sx={{ display: { xs: "none", sm: "block" } }}
          >
            {agent.roomId ? `Room ${agent.roomId} · ` : ""}
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
            onClick={() => (screen ? stopShare() : void startShare())}
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
          <IconButton aria-label="Retry connection" onClick={connect}>
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
      {(failure || chatError || agent.error) && (
        <Alert severity="error" onClose={closeAlert}>
          {failure ?? chatError ?? agent.error?.message}
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
                key={reaction.key}
                aria-label={reaction.emoji}
                className="meet-reaction"
                onAnimationEnd={() => dismissReaction(reaction.key)}
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
                onClick={() => setMicOn((value) => !value)}
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
                onClick={() => setCameraOn((value) => !value)}
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
          className="h-[180px] min-h-0 w-full shrink-0 lg:h-full lg:w-52"
        >
          <Typography variant="overline" color="text.secondary">
            Participants on this page · {page?.participants.length ?? "…"}
          </Typography>
          <Stack direction="row" spacing={1}>
            <Button
              size="small"
              onClick={firstPage}
              disabled={
                !cursor.length && catalogState?.status !== WatchStatus.Failed
              }
            >
              First page
            </Button>
            <Button
              size="small"
              disabled={!page?.hasMore}
              onClick={() => {
                if (page?.hasMore)
                  setCatalogScope((scope) => ({
                    ...scope,
                    room,
                    cursor: [...page.nextCursor],
                  }));
              }}
            >
              Next page
            </Button>
          </Stack>
          {!catalogState || catalogState.status === WatchStatus.Loading ? (
            <Typography role="status">Loading participants…</Typography>
          ) : null}
          {catalogState?.status === WatchStatus.Failed && (
            <Alert severity="error">
              {cursor.length
                ? "This continuation is invalid or expired. Return to the First page."
                : "Unable to load participants."}{" "}
              {catalogState.error?.message}
            </Alert>
          )}
          {page && (
            <Typography variant="caption">
              {page.hasMore
                ? "More participants are available."
                : "End of this page list."}
            </Typography>
          )}
          <Stack
            direction={{ xs: "row", lg: "column" }}
            spacing={1}
            sx={{ overflow: "auto", flex: 1, minHeight: 0 }}
          >
            {page?.participants.map((participant) => (
              <Chip
                key={participant.id}
                size="small"
                label={
                  participant.id === agent.participantId
                    ? `${participant.id} (you)`
                    : participant.id
                }
              />
            ))}
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
            <Stack
              role="log"
              aria-label="Chat transcript"
              spacing={1.5}
              className="min-h-0 flex-1 overflow-y-auto p-4"
            >
              {!messages.length && (
                <Typography variant="body2" color="text.secondary">
                  No messages yet
                </Typography>
              )}
              {messages.map((message) =>
                message.type === "gap" ? (
                  <Alert
                    key={`gap:${message.publicationId}:${message.expectedSequence}:${message.availableSequence}`}
                    severity="warning"
                  >
                    Some messages could not be recovered.
                  </Alert>
                ) : (
                  <Box
                    key={
                      message.type === "local"
                        ? message.key
                        : `${message.publicationId}:${message.sequence}`
                    }
                    className={`max-w-[90%] ${message.participantId === agent.participantId ? "self-end" : "self-start"}`}
                  >
                    <Typography variant="caption" color="text.secondary">
                      {message.participantId === agent.participantId
                        ? "You"
                        : message.participantId}
                    </Typography>
                    <Paper
                      elevation={0}
                      className={`break-words px-3 py-2 ${message.participantId === agent.participantId ? "bg-blue-700! text-white!" : "bg-slate-100!"}`}
                    >
                      {message.value}
                    </Paper>
                  </Box>
                ),
              )}
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
                disabled={!draft.trim() || sending || !agent.participantId}
              >
                <Icons.Send size={20} />
              </IconButton>
            </Stack>
          </Paper>
        )}
      </Box>
    </Box>
  );
}
