import {
  Audio,
  Data,
  Publish,
  Video,
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
const camera = agent.video("camera");
// @ts-expect-error React no longer exports descriptors.
import { track } from "@pulsebeam/react";
// @ts-expect-error Descriptor types are not part of the packed React surface.
import type { TrackSpec } from "@pulsebeam/react";
// @ts-expect-error React does not expose descriptor lookups.
agent.track({ kind: "video", label: "camera" });
// @ts-expect-error Local media mutations belong to Publish.
camera.setEnabled(false);
// @ts-expect-error Local media does not expose capture sources.
camera.source;
// @ts-expect-error Kind-specific sources must remain safe in packed declarations.
Publish({ track: camera, source: capture.audio });
const remote = agent.remote.participant("alice");
const messages = remote.data("messages", "lossy");
Data({
  track: messages,
  async onEvent(event) {
    const bytes: Uint8Array = event.data;
    // @ts-expect-error Lossy events cannot contain a gap.
    event.from;
    await Promise.resolve(bytes);
  },
});
// @ts-expect-error Remote data handles cannot send.
messages.send(new Uint8Array());
// @ts-expect-error Data delivery is required.
agent.data("messages");
Data({
  track: agent.data("chat", "ordered"),
  onEvent(event) {
    if (event.type === "gap") {
      const from: bigint = event.from;
      void from;
    }
  },
});
const settings: VideoCaptureSettings | undefined = capture.video?.settings;
Publish({ track: camera, source: capture.video });
Video({ source: capture.video });
Video({
  source: agent.remote.participant("alice").video("camera"),
  onPlaybackError: (failure: PlaybackError) => void failure.retry(),
});
Audio({ agent });
void settings;
agent.connect();
agent.disconnect();
