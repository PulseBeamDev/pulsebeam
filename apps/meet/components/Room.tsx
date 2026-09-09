import { useCallback, useEffect, useRef, useState } from "react";
import { AgentProvider, createAgent, useAgent } from "@pulsebeam/react";
import type { Agent, RemoteTrack } from "@pulsebeam/react";
import { Button, Input } from "./ui";
import { LocalVideo } from "./LocalVideo";
import { RemoteMedia, type PlaybackRetry } from "./RemoteMedia";
import { useRoomMedia, stopMedia } from "@/hooks/room-media";
import { useTopics } from "@/hooks/topics";
import { useVideoLayout } from "@/hooks/video-layout";
import { desiredState } from "@/lib/model";

export function Room({
  roomId,
  endpoint,
  stream,
  onLeave,
}: {
  roomId: string;
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
      roomId,
      topology: {
        localVideo: ["camera", "screen"],
        localAudio: ["microphone"],
        remoteVideo: 7,
        remoteAudio: 3,
      },
    });
    // eslint-disable-next-line react-hooks/set-state-in-effect -- an agent is effect-owned to prevent Strict Mode reuse after close.
    setAgent(fresh);
    return () => {
      fresh.close();
      stopTimer.current = setTimeout(() => stopMedia(stream), 0);
    };
  }, [endpoint, roomId, stream]);
  return agent ? (
    <AgentProvider agent={agent}>
      <RoomSession roomId={roomId} stream={stream} onLeave={onLeave} />
    </AgentProvider>
  ) : (
    <main className="grid h-dvh place-items-center">Joining room…</main>
  );
}

function RoomSession({
  roomId,
  stream,
  onLeave,
}: {
  roomId: string;
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
    <div className="flex h-dvh flex-col bg-background">
      <header className="meet-room-header flex items-center justify-between border-b">
        <div className="flex min-w-0 items-center gap-2">
          <b className="meet-room-name truncate">Room {roomId}</b>
          <span className="meet-connection-status text-xs text-muted-foreground">
            {agent.connection}
          </span>
        </div>
        <div className="meet-room-actions flex items-center gap-2">
          <label className="meet-latency-label text-xs">
            Latency{" "}
            <select
              aria-label="Latency mode"
              defaultValue="Auto"
              onChange={(event) =>
                setLatency(
                  event.target.value === "Smooth"
                    ? { mode: "fixed", minMs: 400, maxMs: 800 }
                    : event.target.value === "Balanced"
                      ? { mode: "fixed", minMs: 100, maxMs: 200 }
                      : event.target.value === "Zero"
                        ? { mode: "fixed", minMs: 0, maxMs: 0 }
                        : undefined,
                )
              }
            >
              <option>Auto</option>
              <option>Smooth</option>
              <option>Balanced</option>
              <option value="Zero">Zero</option>
            </select>
          </label>
          <Button
            size="sm"
            onClick={() => (screen ? detachScreen() : void startShare())}
          >
            {screen ? "Stop share" : "Share"}
          </Button>
          <Button size="sm" onClick={() => setChatOpen((open) => !open)}>
            Chat
          </Button>
          <Button
            size="sm"
            className="meet-reconnect"
            onClick={agent.reconnect}
          >
            Reconnect
          </Button>
          <Button size="sm" variant="destructive" onClick={onLeave}>
            End
          </Button>
        </div>
      </header>
      {blocked && (
        <p role="alert" className="flex gap-2 p-2 text-sm text-destructive">
          {blocked}
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
        </p>
      )}
      <main className="meet-room-main flex min-h-0 flex-1">
        <section className="meet-spotlight relative flex min-h-0 flex-1 items-center justify-center overflow-hidden rounded-lg bg-black">
          <div
            ref={spotlightFrame}
            className="meet-spotlight-frame relative w-full"
          >
            {spotlight && agent.tracks[spotlight]?.kind === "video" ? (
              <RemoteMedia
                track={agent.tracks[spotlight]}
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
              <span className="absolute top-3 left-3 rounded bg-black/60 px-2 py-1 text-xs text-white">
                Spotlight: {label(spotlight)}
              </span>
            )}
            {reactions.map((reaction) => (
              <span
                key={reaction.id}
                className="absolute bottom-8 left-1/2 animate-[floatUp_3s_ease-out] text-4xl"
              >
                {reaction.emoji}
              </span>
            ))}
            <div className="absolute bottom-3 left-1/2 flex -translate-x-1/2 gap-2">
              <Button size="sm" onClick={() => toggle("microphone", !micOn)}>
                {micOn ? "Mute" : "Unmute"}
              </Button>
              <Button size="sm" onClick={() => toggle("camera", !cameraOn)}>
                {cameraOn ? "Camera off" : "Camera on"}
              </Button>
              {["👍", "❤️", "😂", "😮", "👏", "🔥"].map((emoji) => (
                <Button
                  key={emoji}
                  size="sm"
                  onClick={() => sendReaction(emoji)}
                >
                  {emoji}
                </Button>
              ))}
            </div>
          </div>
        </section>
        <aside className="meet-participants flex shrink-0">
          <div className="meet-participant-scroll min-h-0 flex-1">
            <div className="meet-participant-list flex gap-2">
              <button
                className="meet-participant-tile relative aspect-video overflow-hidden rounded border"
                onClick={() => setPin(null)}
              >
                <LocalVideo
                  stream={stream}
                  mirror
                  className="h-full w-full object-cover"
                />
                <span className="absolute bottom-1 left-1 text-xs">You</span>
              </button>
              {remotePublications.map((publication) => (
                <button
                  key={publication.id}
                  ref={(element) => {
                    if (element) tiles.current.set(publication.id, element);
                    else tiles.current.delete(publication.id);
                  }}
                  data-publication={publication.id}
                  className="meet-participant-tile relative aspect-video overflow-hidden rounded border"
                  onClick={() => setPin(publication.id)}
                >
                  {agent.tracks[publication.id]?.kind === "video" ? (
                    <RemoteMedia
                      track={agent.tracks[publication.id]}
                      onBlocked={onBlocked}
                    />
                  ) : (
                    <span className="grid h-full place-items-center text-xs">
                      Waiting for video
                    </span>
                  )}
                  <span className="absolute bottom-1 left-1 text-xs">
                    {publication.participantId}
                  </span>
                </button>
              ))}
            </div>
          </div>
        </aside>
        {chatOpen && (
          <aside className="w-72 shrink-0 border-l p-3">
            <div className="h-full overflow-auto">
              {messages.map((message) => (
                <p key={message.id}>
                  <b>{message.self ? "You" : message.sender}:</b> {message.text}
                </p>
              ))}
            </div>
            <form
              onSubmit={(event) => {
                event.preventDefault();
                sendChat(draft);
                setDraft("");
              }}
              className="flex gap-2"
            >
              <Input
                aria-label="Message"
                value={draft}
                onChange={(event) => setDraft(event.target.value)}
              />
              <Button type="submit">Send</Button>
            </form>
          </aside>
        )}
      </main>
      {audioTracks.map((track) => (
        <RemoteMedia
          key={track.publicationId}
          track={track}
          onBlocked={onBlocked}
        />
      ))}
    </div>
  );
}
