import {
  useAgent,
  useUserMedia,
  Video,
  type Agent,
  type AgentConfig,
  type PlaybackError,
  type VideoSource,
  type PublishableVideoSource,
  type VideoCaptureSettings,
} from "@pulsebeam/react";

const config: AgentConfig = {
  endpoint: "https://pulsebeam.example",
  token: "opaque-token",
  topology: { localVideos: 1, localAudios: 1 },
};
const agent: Agent = useAgent(config);
const camera = useUserMedia({ video: true, audio: false });
const Camera = agent.local.video("camera");
const publicationSource: PublishableVideoSource | null = camera.video;
const settings: VideoCaptureSettings | undefined = camera.video?.settings;
Camera({ source: publicationSource });
const sources: readonly (VideoSource | null)[] = [
  camera.video,
  Camera,
  agent.remote.participant("alice").video("camera"),
];
for (const source of sources) Video({ source });
void settings;
Video({
  source: agent.remote.videoTracks[0] ?? null,
  onPlaybackError: (failure: PlaybackError) => void failure.retry(),
});
void agent.remote.resumeAudio();
agent.connect();
agent.disconnect();
