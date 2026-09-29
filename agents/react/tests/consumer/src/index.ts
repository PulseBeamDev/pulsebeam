import {
  useAgent,
  useUserMedia,
  Video,
  Audio,
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
  agent.localVideoTrack("camera").setSource(camera.videoTrack);
  Video({
    source: agent.remoteVideoTracks[0] ?? null,
    onPlaybackError: (failure: PlaybackError) => void failure.retry(),
  });
  Audio({ source: agent.remoteAudio });
  agent.connect();
  agent.disconnect();
}
