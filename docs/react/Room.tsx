import { useEffect, useState } from "react";
import {
  Video,
  useAgent,
  useUserMedia,
  type AgentConfig,
  type PlaybackError,
} from "@pulsebeam/react";

export function Room({ config }: { config: AgentConfig }) {
  const agent = useAgent(config);
  const capture = useUserMedia({ video: true, audio: true });
  const [error, setError] = useState("");
  const [playback, setPlayback] = useState<PlaybackError | null>(null);
  const report = (failure: unknown) => setError(String(failure));
  const camera = agent.local.video("camera");
  const microphone = agent.local.audio("microphone");

  useEffect(() => {
    camera.setSource(capture.videoTrack);
    microphone.setSource(capture.audioTrack);
  }, [camera, microphone, capture.videoTrack, capture.audioTrack]);

  const snapshot = agent.getSnapshot();
  return (
    <main>
      <h1>PulseBeam React room</h1>
      <p role="status">
        Connection: {snapshot?.connection ?? "preparing"}; participant:{" "}
        {snapshot?.participantExternalId ?? "not admitted"}
      </p>
      <p>Capture: {capture.state}</p>
      <button disabled={!agent} onClick={() => agent.connect()}>
        Join
      </button>
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
      <button
        disabled={!agent}
        onClick={() => void agent.remote.resumeAudio().catch(report)}
      >
        Enable remote audio
      </button>
      <h2>Local preview</h2>
      <Video
        source={capture.videoTrack}
        mirror
        style={{ width: 320, height: 240 }}
        onPlaybackError={setPlayback}
      />
      <h2>Remote video ({agent.remote.videoTracks.length ?? 0})</h2>
      {agent?.remote.videoTracks.map((track) => (
        <section key={`${track.participantId}:${track.label}`}>
          <p>
            {track.participantId}: {track.label}
          </p>
          <Video
            source={track}
            style={{ width: 320, height: 240 }}
            onPlaybackError={setPlayback}
          />
        </section>
      ))}
      <p>Remote audio publications: {agent?.remote.audioTracks.length ?? 0}</p>
      {playback && (
        <button
          onClick={() =>
            void playback
              .retry()
              .then(() => setPlayback(null))
              .catch(report)
          }
        >
          Retry video playback
        </button>
      )}
      {(error || capture.error || snapshot?.failure) && (
        <p role="alert">
          {error || capture.error?.message || snapshot?.failure?.message}
        </p>
      )}
    </main>
  );
}
