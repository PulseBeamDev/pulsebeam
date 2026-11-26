import { DataEvent } from "@pulsebeam/react";
import {
  Audio,
  Data,
  usePublish,
  useSubscribe,
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
};
const maybeAgent: Agent | null = useAgent(config);
const agent: Agent = maybeAgent!;
const capture = useCapture("device", { video: true, audio: false });
const camera = agent.video("camera");
// @ts-expect-error No public publication component.
import { Publish } from "@pulsebeam/react";
// @ts-expect-error React no longer exports descriptors.
import { track } from "@pulsebeam/react";
// @ts-expect-error Descriptor types are not part of the packed React surface.
import type { TrackSpec } from "@pulsebeam/react";
// @ts-expect-error React does not expose descriptor lookups.
agent.track({ kind: "video", trackId: "camera" });
// @ts-expect-error Local media mutations belong to Publish.
camera.setEnabled(false);
// @ts-expect-error Local media does not expose capture sources.
camera.source;
// @ts-expect-error Kind-specific sources must remain safe in packed declarations.
usePublish(camera, capture.audio);
const remote = agent.room("selected-room").participant("alice");
const messages = remote.data("messages");
Data({
  track: messages,
  async onEvent(event) {
    if (!DataEvent.Message.instanceOf(event)) return;
    const bytes: Uint8Array = event.inner.data;
    // @ts-expect-error Messages do not carry gap bounds.
    event.inner.from;
    await Promise.resolve(bytes);
  },
});
// @ts-expect-error Remote data handles cannot send.
messages.send(new Uint8Array());
// @ts-expect-error Delivery belongs on the owner.
agent.data("messages", "lossy");
Data({
  track: agent.room("selected-room").participant("alice").data("chat"),
  onEvent(event) {
    if (DataEvent.Discontinuity.instanceOf(event)) {
      const from: string = event.inner.previousPublicationId;
      void from;
    }
  },
});
const settings: VideoCaptureSettings | undefined = capture.video?.settings;
usePublish(camera, capture.video);
Video({ source: capture.video });
Video({
  source: useSubscribe(
    agent.room("selected-room").participant("alice").video("camera"),
  ),
  onPlaybackError: (failure: PlaybackError) => void failure.retry(),
});
Audio({ source: useSubscribe(remote.audio("microphone")) });
// @ts-expect-error Logical refs cannot implicitly create a receive owner.
Video({ source: remote.video("camera") });
// @ts-expect-error No agent-wide implicit audio playback.
Audio({ agent });
void settings;
agent.join("explicit-room");
agent.renew("replacement-token");
