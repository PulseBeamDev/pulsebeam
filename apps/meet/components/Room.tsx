import { useCallback, useEffect, useRef, useState } from "react";
import { AgentProvider, createAgent, useAgent } from "@pulsebeam/react";
import type { Agent, RemoteTrack } from "@pulsebeam/react";
import {
  Badge,
  Button,
  Card,
  Input,
  ScrollArea,
  Separator,
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
  cn,
} from "./ui";
import {
  Gauge,
  Loader2,
  MessageCircle,
  Mic,
  MicOff,
  Monitor,
  MonitorOff,
  PhoneOff,
  RotateCcw,
  Send,
  SmilePlus,
  Video as VideoIcon,
  VideoOff,
  X,
} from "lucide-react";
import { LocalVideo } from "./LocalVideo";
import { RemoteMedia, type PlaybackRetry } from "./RemoteMedia";
import { useRoomMedia, stopMedia } from "@/hooks/room-media";
import { useTopics } from "@/hooks/topics";
import { useVideoLayout } from "@/hooks/video-layout";
import { desiredState } from "@/lib/model";

const latencyModes = [
  {
    label: "Smooth",
    minMs: 400,
    maxMs: 800,
    description: "400–800 ms",
  },
  {
    label: "Balanced",
    minMs: 100,
    maxMs: 200,
    description: "100–200 ms",
  },
  { label: "Zero", minMs: 0, maxMs: 0, description: "Render immediately" },
] as const;

const reactionEmojis = ["👍", "❤️", "😂", "😮", "👏", "🔥"];

export function Room({
  token,
  endpoint,
  stream,
  onLeave,
}: {
  token: string;
  endpoint: string;
  stream: MediaStream;
  onLeave(): void;
}) {
  const [agent, setAgent] = useState<Agent | null>(null);
  const stopTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    if (stopTimer.current) clearTimeout(stopTimer.current);
    const fresh = createAgent({
      endpoint,
      token,
      topology: {
        localVideo: ["camera", "screen"],
        localAudio: ["microphone"],
        remoteVideo: 7,
        remoteAudio: 3,
      },
      logging: { level: "debug" },
    });
    // eslint-disable-next-line react-hooks/set-state-in-effect -- an agent is effect-owned to prevent Strict Mode reuse after close.
    setAgent(fresh);
    return () => {
      fresh.close();
      stopTimer.current = setTimeout(() => stopMedia(stream), 0);
    };
  }, [endpoint, token, stream]);
  return agent ? (
    <AgentProvider agent={agent}>
      <RoomSession agent={agent} stream={stream} onLeave={onLeave} />
    </AgentProvider>
  ) : (
    <main className="grid h-dvh place-items-center">Joining room…</main>
  );
}

function RoomSession({
  agent: playbackAgent,
  stream,
  onLeave,
}: {
  agent: Agent;
  stream: MediaStream;
  onLeave(): void;
}) {
  const agent = useAgent();
  const [latency, setLatency] = useState<{
    mode: "fixed";
    minMs: number;
    maxMs: number;
  }>();
  const [draft, setDraft] = useState("");
  const [chatOpen, setChatOpen] = useState(false);
  const [reactionPickerOpen, setReactionPickerOpen] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [playbackRetry, setPlaybackRetry] = useState<PlaybackRetry | null>(
    null,
  );
  const { screen, cameraOn, micOn, detachScreen, startShare, toggle } =
    useRoomMedia(agent, stream, setFailure);
  const {
    messages,
    reactions,
    sendChat,
    sendReaction,
    error: topicError,
    clearError,
  } = useTopics();
  const {
    remotePublications,
    publicationById,
    spotlight,
    selected,
    tiles,
    spotlightFrame,
    setPin,
  } = useVideoLayout(agent.publications, agent.participantId);
  const onBlocked = useCallback((reason: string, retry: PlaybackRetry) => {
    setFailure(`Playback: ${reason}`);
    setPlaybackRetry(() => retry);
  }, []);
  useEffect(() => {
    agent.setState(
      desiredState(
        true,
        ["camera", "microphone", ...(screen ? ["screen"] : [])],
        selected.map(({ id, slot, height, priority }) => ({
          slot,
          trackId: id,
          height,
          minHeight: priority === 200 ? 360 : 90,
          minFps: 15,
          priority,
        })),
        latency,
      ),
    );
  }, [agent, latency, screen, selected]);
  const blocked = failure ?? topicError ?? agent.failure?.message;
  const audioTracks = agent.audio
    .map((binding) => agent.tracks[binding.trackId])
    .filter((track): track is RemoteTrack => track?.kind === "audio");
  const label = (id: string) => publicationById.get(id)?.participantId ?? id;
  return (
    <TooltipProvider>
      <div className="flex h-dvh flex-col overflow-hidden bg-background font-sans">
        <header className="meet-room-header z-20 flex shrink-0 items-center justify-between gap-2 border-b bg-card/50 backdrop-blur-md">
          <div className="flex min-w-0 items-center gap-2">
            <Badge variant="outline" className="gap-2 px-2 py-0.5">
              <span
                aria-hidden="true"
                className={cn(
                  "h-1.5 w-1.5 rounded-full",
                  agent.connection === "connected"
                    ? "animate-pulse bg-emerald-500"
                    : "bg-amber-500",
                )}
              />
              <span className="meet-room-name truncate text-xs font-medium text-muted-foreground">
                Participant:{" "}
                <span className="text-foreground">
                  {agent.participantId ?? "connecting"}
                </span>
                <span className="sr-only">, {agent.connection}</span>
              </span>
            </Badge>
            {agent.connection !== "connected" &&
              agent.connection !== "disconnected" &&
              agent.connection !== "terminal-failure" && (
                <Badge
                  variant="secondary"
                  className="meet-connection-status hidden gap-2"
                >
                  <Loader2 className="h-3 w-3 animate-spin text-primary" />
                  <span className="text-xs font-medium">Connecting…</span>
                </Badge>
              )}
          </div>
          <div className="meet-room-actions flex items-center gap-1">
            <div className="flex h-8 min-w-0 items-center gap-1.5 rounded-md border border-border/60 bg-muted/40 px-2">
              <Gauge className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
              <span className="meet-latency-label text-xs text-muted-foreground">
                Latency
              </span>
              <select
                aria-label="Latency mode"
                value={
                  latency
                    ? latencyModes.find(
                        (mode) =>
                          mode.minMs === latency.minMs &&
                          mode.maxMs === latency.maxMs,
                      )?.label
                    : "Auto"
                }
                onChange={(event) => {
                  const mode = latencyModes.find(
                    (candidate) => candidate.label === event.target.value,
                  );
                  if (mode)
                    setLatency({
                      mode: "fixed",
                      minMs: mode.minMs,
                      maxMs: mode.maxMs,
                    });
                }}
                className="h-6 min-w-18 border-0 bg-transparent px-0 text-xs font-medium outline-none"
              >
                <option value="Auto" disabled={Boolean(latency)}>
                  {latency ? "Auto (new room)" : "Auto"}
                </option>
                {latencyModes.map((mode) => (
                  <option key={mode.label} value={mode.label}>
                    {mode.label} — {mode.description}
                  </option>
                ))}
              </select>
            </div>
            <Separator orientation="vertical" className="mx-1 h-4" />
            <Button
              variant="ghost"
              size="sm"
              className={cn(
                "h-8 rounded-md px-2.5",
                screen && "bg-primary/10 text-primary",
              )}
              onClick={() => (screen ? detachScreen() : void startShare())}
            >
              {screen ? (
                <MonitorOff className="h-4 w-4" />
              ) : (
                <Monitor className="h-4 w-4" />
              )}
              <span className="text-xs">{screen ? "Stop" : "Share"}</span>
            </Button>
            <Button
              variant="ghost"
              size="sm"
              className={cn(
                "h-8 rounded-md px-2.5",
                chatOpen && "bg-primary/10 text-primary",
              )}
              aria-pressed={chatOpen}
              onClick={() => setChatOpen((open) => !open)}
            >
              <MessageCircle className="h-4 w-4" />
              <span className="text-xs">Chat</span>
            </Button>
            <Button
              size="sm"
              variant="destructive"
              className="h-8 px-2.5 text-xs"
              onClick={onLeave}
            >
              <PhoneOff className="h-3.5 w-3.5" /> End
            </Button>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  size="icon"
                  variant="ghost"
                  className="meet-reconnect h-8 w-8"
                  aria-label="Reconnect"
                  onClick={agent.reconnect}
                >
                  <RotateCcw className="h-3.5 w-3.5" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Reconnect</TooltipContent>
            </Tooltip>
          </div>
        </header>
        {blocked && (
          <div
            role="alert"
            className="flex flex-wrap items-center gap-2 border-b border-destructive/20 bg-destructive/10 px-3 py-2 text-sm text-destructive"
          >
            <span className="min-w-0 flex-1">{blocked}</span>
            {playbackRetry && (
              <Button
                size="sm"
                variant="secondary"
                onClick={() => playbackRetry()}
              >
                Retry playback
              </Button>
            )}
            <Button
              size="sm"
              variant="secondary"
              onClick={() => {
                setFailure(null);
                setPlaybackRetry(null);
                clearError();
              }}
            >
              Dismiss
            </Button>
          </div>
        )}
        <main className="meet-room-main relative flex min-h-0 flex-1">
          <Card className="meet-spotlight relative flex min-h-0 flex-1 items-center justify-center overflow-hidden bg-black py-0">
            <div
              ref={spotlightFrame}
              className="meet-spotlight-frame relative w-full"
            >
              {spotlight && agent.tracks[spotlight]?.kind === "video" ? (
                <RemoteMedia
                  agent={playbackAgent}
                  publicationId={spotlight}
                  kind="video"
                  onBlocked={onBlocked}
                />
              ) : (
                <LocalVideo
                  stream={stream}
                  mirror
                  className="h-full w-full object-contain"
                />
              )}
              {spotlight && (
                <Badge className="absolute top-3 left-3 h-7 max-w-48 gap-2 truncate rounded-lg border border-white/10 bg-black/60 px-2.5 py-1 text-[9px] font-medium text-white backdrop-blur-md">
                  <span className="h-1.5 w-1.5 rounded-full bg-primary" />
                  {label(spotlight)}
                </Badge>
              )}
              {reactions.map((reaction, index) => (
                <span
                  key={reaction.id}
                  className="absolute bottom-24 animate-[floatUp_3s_ease-out_forwards] text-4xl"
                  style={{ left: `${24 + (index % 5) * 13}%` }}
                >
                  {reaction.emoji}
                </span>
              ))}
              <div className="absolute bottom-3 left-1/2 flex -translate-x-1/2 gap-2 rounded-xl border border-white/10 bg-black/60 p-1.5 shadow-2xl backdrop-blur-md sm:bottom-6 sm:gap-3">
                <Button
                  size="icon"
                  variant={micOn ? "secondary" : "destructive"}
                  className="h-11 w-11 sm:h-10 sm:w-10"
                  aria-label={micOn ? "Mute microphone" : "Unmute microphone"}
                  onClick={() => toggle("microphone", !micOn)}
                >
                  {micOn ? (
                    <Mic className="h-4 w-4" />
                  ) : (
                    <MicOff className="h-4 w-4" />
                  )}
                </Button>
                <Button
                  size="icon"
                  variant={cameraOn ? "secondary" : "destructive"}
                  className="h-11 w-11 sm:h-10 sm:w-10"
                  aria-label={cameraOn ? "Turn camera off" : "Turn camera on"}
                  onClick={() => toggle("camera", !cameraOn)}
                >
                  {cameraOn ? (
                    <VideoIcon className="h-4 w-4" />
                  ) : (
                    <VideoOff className="h-4 w-4" />
                  )}
                </Button>
                <div className="relative">
                  <Button
                    size="icon"
                    variant="secondary"
                    className="h-11 w-11 sm:h-10 sm:w-10"
                    aria-label="Send a reaction"
                    aria-expanded={reactionPickerOpen}
                    onClick={() => setReactionPickerOpen((open) => !open)}
                  >
                    <SmilePlus className="h-4 w-4" />
                  </Button>
                  {reactionPickerOpen && (
                    <div className="absolute bottom-full left-1/2 mb-2 flex -translate-x-1/2 gap-1 rounded-xl border border-white/10 bg-black/80 px-2 py-1.5 backdrop-blur-md">
                      {reactionEmojis.map((emoji) => (
                        <button
                          key={emoji}
                          className="rounded p-1 text-xl transition-transform hover:scale-125 focus-visible:ring-2 focus-visible:ring-white active:scale-110"
                          aria-label={`React with ${emoji}`}
                          onClick={() => {
                            sendReaction(emoji);
                            setReactionPickerOpen(false);
                          }}
                        >
                          {emoji}
                        </button>
                      ))}
                    </div>
                  )}
                </div>
              </div>
            </div>
          </Card>
          <aside className="meet-participants flex shrink-0 flex-col">
            <div className="flex items-center justify-between px-1">
              <p className="text-[10px] font-bold tracking-widest text-muted-foreground uppercase">
                Participants
              </p>
              <Badge variant="secondary" className="h-4 text-[9px]">
                {remotePublications.length + 1}
              </Badge>
            </div>
            <div className="meet-participant-scroll min-h-0 flex-1">
              <div className="meet-participant-list flex gap-2">
                {spotlight && (
                  <button
                    className="meet-participant-tile relative aspect-video shrink-0 overflow-hidden rounded-lg border-2 border-transparent bg-muted transition-colors hover:border-primary"
                    aria-label="Spotlight your camera"
                    onClick={() => setPin("local")}
                  >
                    <LocalVideo
                      stream={stream}
                      mirror
                      className="h-full w-full object-cover"
                    />
                    <Badge
                      variant="secondary"
                      className="absolute bottom-1.5 left-1.5 h-4 border-0 bg-black/50 text-[9px] text-white backdrop-blur-sm"
                    >
                      You
                    </Badge>
                  </button>
                )}
                {remotePublications
                  .filter((publication) => publication.id !== spotlight)
                  .map((publication) => (
                    <button
                      key={publication.id}
                      ref={(element) => {
                        if (element) tiles.current.set(publication.id, element);
                        else tiles.current.delete(publication.id);
                      }}
                      data-publication={publication.id}
                      className="meet-participant-tile relative aspect-video shrink-0 overflow-hidden rounded-lg border-2 border-transparent bg-muted transition-colors hover:border-primary"
                      aria-label={`Spotlight ${publication.participantId}`}
                      onClick={() => setPin(publication.id)}
                    >
                      {agent.tracks[publication.id]?.kind === "video" ? (
                        <RemoteMedia
                          agent={playbackAgent}
                          publicationId={publication.id}
                          kind="video"
                          onBlocked={onBlocked}
                        />
                      ) : (
                        <span className="grid h-full place-items-center text-xs">
                          Waiting for video
                        </span>
                      )}
                      <Badge
                        variant="secondary"
                        className="absolute bottom-1.5 left-1.5 h-4 max-w-[calc(100%-0.75rem)] truncate border-0 bg-black/50 text-[9px] text-white backdrop-blur-sm"
                      >
                        {publication.participantId}
                      </Badge>
                    </button>
                  ))}
              </div>
            </div>
          </aside>
          {chatOpen && (
            <aside className="meet-chat flex w-72 shrink-0 flex-col border-l bg-card">
              <div className="flex items-center gap-2 border-b px-3 py-2">
                <MessageCircle className="h-4 w-4 text-muted-foreground" />
                <span className="flex-1 text-sm font-medium">Chat</span>
                <Button
                  size="icon"
                  variant="ghost"
                  className="size-7"
                  aria-label="Close chat"
                  onClick={() => setChatOpen(false)}
                >
                  <X className="size-3.5" />
                </Button>
              </div>
              <ScrollArea className="min-h-0 flex-1 px-3 py-2">
                <div className="flex flex-col gap-2">
                  {messages.length === 0 && (
                    <p className="py-8 text-center text-xs text-muted-foreground">
                      No messages yet
                    </p>
                  )}
                  {messages.map((message) => (
                    <div
                      key={message.id}
                      className={cn(
                        "flex flex-col gap-0.5",
                        message.self ? "items-end" : "items-start",
                      )}
                    >
                      <span className="px-1 text-[10px] text-muted-foreground">
                        {message.self ? "You" : message.sender}
                      </span>
                      <div
                        className={cn(
                          "max-w-[85%] rounded-2xl px-3 py-1.5 text-sm break-words",
                          message.self
                            ? "rounded-br-sm bg-primary text-primary-foreground"
                            : "rounded-bl-sm bg-muted",
                        )}
                      >
                        {message.text}
                      </div>
                    </div>
                  ))}
                </div>
              </ScrollArea>
              <form
                onSubmit={(event) => {
                  event.preventDefault();
                  sendChat(draft);
                  setDraft("");
                }}
                className="flex gap-2 border-t px-3 py-2"
              >
                <Input
                  aria-label="Message"
                  placeholder="Message…"
                  className="h-8 text-sm"
                  value={draft}
                  onChange={(event) => setDraft(event.target.value)}
                />
                <Button
                  type="submit"
                  size="icon"
                  className="h-8 w-8"
                  aria-label="Send message"
                  disabled={!draft.trim()}
                >
                  <Send className="h-3.5 w-3.5" />
                </Button>
              </form>
            </aside>
          )}
        </main>
        {audioTracks.map((track) => (
          <RemoteMedia
            agent={playbackAgent}
            key={track.publicationId}
            publicationId={track.publicationId}
            kind="audio"
            onBlocked={onBlocked}
          />
        ))}
      </div>
    </TooltipProvider>
  );
}
