import { useState } from "react";
import {
  Audio,
  Publish,
  Video,
  track,
  useAgent,
  useCapture,
  type AgentConfig,
  type PlaybackError,
  type PlaybackFailure,
  type RemoteVideoTrack,
} from "@pulsebeam/react";

const camera = track.video("camera");
const microphone = track.audio("microphone");

export function Room({ config }: { config: AgentConfig }) {
  const agent = useAgent(config);
  const capture = useCapture("device", { video: true, audio: true });
  const [error, setError] = useState("");
  const [videoPlayback, setVideoPlayback] = useState<PlaybackError | null>(
    null,
  );
  const [audioPlayback, setAudioPlayback] = useState<PlaybackFailure | null>(
    null,
  );
  const report = (failure: unknown) => setError(String(failure));
  const remoteVideos = agent.remote.participants.flatMap((participant) =>
    participant.tracks.filter(
      (value): value is RemoteVideoTrack => value.kind === "video",
    ),
  );

  return (
    <main>
      <Publish track={agent.track(camera)} source={capture.video} />
      <Publish track={agent.track(microphone)} source={capture.audio} />
      <Audio agent={agent} onPlaybackError={setAudioPlayback} />
      <h1>PulseBeam React room</h1>
      <p role="status">
        Connection: {agent.connection}; participant:{" "}
        {agent.participantId ?? "not admitted"}
      </p>
      <p>Capture: {capture.state}</p>
      <button onClick={() => agent.connect()}>Join</button>
      <button onClick={() => void capture.request().catch(report)}>
        Enable camera and microphone
      </button>
      <button
        onClick={() => {
          capture.stop();
          agent.disconnect();
        }}
      >
        Leave and stop capture
      </button>
      <h2>Local preview</h2>
      <Video
        source={capture.video}
        mirror
        style={{ width: 320, height: 240 }}
        onPlaybackError={setVideoPlayback}
      />
      <h2>Remote video ({remoteVideos.length})</h2>
      {remoteVideos.map((remote) => (
        <section key={`${remote.participantId}:${remote.label}`}>
          <p>
            {remote.participantId}: {remote.label}
          </p>
          <Video
            source={remote}
            style={{ width: 320, height: 240 }}
            onPlaybackError={setVideoPlayback}
          />
        </section>
      ))}
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
      {(error || capture.error || agent.error) && (
        <p role="alert">
          {error || capture.error?.message || agent.error?.message}
        </p>
      )}
    </main>
  );
}
