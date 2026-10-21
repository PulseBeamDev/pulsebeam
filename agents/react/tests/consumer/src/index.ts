import {
  Audio,
  Publish,
  Video,
  track,
  useAgent,
  useCapture,
  type Agent,
  type AgentConfig,
  type PlaybackError,
  type VideoCaptureSettings,
} from "@pulsebeam/react";

const config: AgentConfig = {
  endpoint: "https://pulsebeam.example",
  token: "opaque-token",
  topology: { localVideos: 1, localAudios: 1 },
};
const agent: Agent = useAgent(config);
const capture = useCapture("device", { video: true, audio: false });
const camera = agent.track(track.video("camera"));
const settings: VideoCaptureSettings | undefined = capture.video?.settings;
Publish({ track: camera, source: capture.video });
Video({ source: capture.video });
Video({
  source: agent.remote.participant("alice").track(track.video("camera")),
  onPlaybackError: (failure: PlaybackError) => void failure.retry(),
});
Audio({ agent });
void settings;
agent.connect();
agent.disconnect();
