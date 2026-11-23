import { useState } from "react";
import {
  AgentStatus,
  Audio,
  usePublish,
  useSubscribe,
  Video,
  useAgent,
  useCapture,
  type Agent,
  type AgentConfig,
  type CaptureResult,
  type PlaybackError,
} from "@pulsebeam/react";

export function Room({
  config,
  room,
  onLeave,
}: {
  config: AgentConfig;
  room: string;
  onLeave(): void;
}) {
  const agent = useAgent(config);
  const capture = useCapture("device", { video: true, audio: true });
  if (!agent) return <p role="status">Preparing session…</p>;
  return (
    <ActiveRoom agent={agent} capture={capture} room={room} onLeave={onLeave} />
  );
}

function ActiveRoom({
  agent,
  capture,
  room,
  onLeave,
}: {
  agent: Agent;
  capture: CaptureResult;
  room: string;
  onLeave(): void;
}) {
  const state = agent.state();
  const [error, setError] = useState("");
  const [videoPlayback, setVideoPlayback] = useState<PlaybackError | null>(
    null,
  );
  const [audioPlayback, setAudioPlayback] = useState<PlaybackError | null>(
    null,
  );
  const report = (failure: unknown) => setError(String(failure));
  const [peer, setPeer] = useState("");
  const remoteVideo = peer
    ? agent.room(room).participant(peer).video("camera")
    : null;

  usePublish(agent.video("camera"), capture.video);
  usePublish(agent.audio("microphone"), capture.audio);
  const video = useSubscribe(remoteVideo);
  const audio = useSubscribe(
    peer ? agent.room(room).participant(peer).audio("microphone") : null,
  );

  return (
    <main>
      <Audio source={audio} onPlaybackError={setAudioPlayback} />
      <h1>PulseBeam React room</h1>
      <p role="status">
        Connection: {AgentStatus[state.status]}; participant:{" "}
        {state.participantId ?? "not admitted"}
      </p>
      <p>Capture: {capture.state}</p>
      <button
        onClick={() => {
          try {
            agent.join(room);
          } catch (error) {
            report(error);
          }
        }}
      >
        Join
      </button>
      <button onClick={() => void capture.request().catch(report)}>
        Enable camera and microphone
      </button>
      <button onClick={onLeave}>Leave and stop capture</button>
      <h2>Local preview</h2>
      <Video
        source={capture.video}
        mirror
        style={{ width: 320, height: 240 }}
        onPlaybackError={setVideoPlayback}
      />
      <h2>Remote video</h2>
      <label>
        Other participant ID
        <input value={peer} onChange={(event) => setPeer(event.target.value)} />
      </label>
      <Video
        source={video}
        style={{ width: 320, height: 240 }}
        onPlaybackError={setVideoPlayback}
      />
      {audioPlayback && (
        <button
          onClick={() =>
            void audioPlayback
              .retry()
              .then(() => setAudioPlayback(null))
              .catch(report)
          }
        >
          Enable remote audio
        </button>
      )}
      {videoPlayback && (
        <button
          onClick={() =>
            void videoPlayback
              .retry()
              .then(() => setVideoPlayback(null))
              .catch(report)
          }
        >
          Retry video playback
        </button>
      )}
      {(error || capture.error || state.error) && (
        <p role="alert">
          {error || capture.error?.message || state.error?.message}
        </p>
      )}
    </main>
  );
}
