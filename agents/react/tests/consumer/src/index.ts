import {
  useAgent,
  useUserMedia,
  Video,
  type Agent,
  type AgentConfig,
  type PlaybackError,
} from "@pulsebeam/react";

const config: AgentConfig = {
  endpoint: "https://pulsebeam.example",
  token: "opaque-token",
  topology: { localVideos: 1, localAudios: 1 },
};
const agent: Agent | null = useAgent(config);
const camera = useUserMedia({ video: true, audio: false });
if (agent) {
  agent.local.video("camera").setSource(camera.videoTrack);
  Video({
    source: agent.remote.videoTracks[0] ?? null,
    onPlaybackError: (failure: PlaybackError) => void failure.retry(),
  });
  void agent.remote.resumeAudio();
  agent.connect();
  agent.disconnect();
}
