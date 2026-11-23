import { DataDelivery, DataEvent } from "@pulsebeam/react";
import {
  Audio,
  Data,
  Video,
  useAgent,
  useCapture,
  useDevices,
  type Agent,
  type AgentConfig,
  type CameraDevice,
  type MicrophoneDevice,
  type VideoCaptureSettings,
  type AudioCaptureSettings,
} from "@pulsebeam/react";
import {
  Agent as WebAgent,
  type VideoPublication,
  type AudioPublication,
  type DataPublication,
  type VideoSubscription,
  type AudioSubscription,
  type DataSubscription,
  type VideoSource,
  type AudioSource,
  type Catalog as WebCatalog,
  LogLevel,
} from "@pulsebeam/web";
import {
  usePublish,
  useSubscribe,
  useWatch,
  type Catalog,
  type WatchConfig,
} from "@pulsebeam/react";
import { createElement } from "react";

// @ts-expect-error React owns Agents only through useAgent.
import { createAgent as createReactAgent } from "@pulsebeam/react";
// @ts-expect-error Managed lifecycle is not public Web API.
import { createManagedAgent } from "@pulsebeam/web";
// @ts-expect-error The publication component was replaced by actual-handle hooks.
import { Publish } from "@pulsebeam/react";
// @ts-expect-error No provider hierarchy.
import { AgentProvider } from "@pulsebeam/react";
// @ts-expect-error Replaced capture hooks are absent.
import { useUserMedia } from "@pulsebeam/react";
// @ts-expect-error Replaced capture hooks are absent.
import { useDisplayMedia } from "@pulsebeam/react";
// @ts-expect-error Replaced device hook is absent.
import { useMediaDevices } from "@pulsebeam/react";

const config: AgentConfig = {
  endpoint: "https://pulsebeam.example",
  token: "opaque-token",
  logLevel: LogLevel.Info,
};
const nullableAgent: Agent | null = useAgent(config);
const agent: Agent = nullableAgent!;
// Check identity in both directions, not just assignability to a narrowed facade.
type Equal<A, B> =
  (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
    ? true
    : false;
type Expect<T extends true> = T;
type AgentIdentity = Expect<
  Equal<ReturnType<typeof useAgent>, WebAgent | null>
>;
const actualAgent: WebAgent = nullableAgent!;
type WatchIdentity = Expect<
  Equal<ReturnType<typeof useWatch>, WebCatalog | null>
>;
const watchConfig: WatchConfig = { limit: 7, cursor: Uint8Array.of(1) };
const watch: Catalog | null = useWatch(
  actualAgent.room("pinned-room"),
  watchConfig,
);
const absentWatch: WebCatalog | null = useWatch(null);
void [watch, absentWatch];
declare const videoSource: VideoSource;
declare const audioSource: AudioSource;
const videoPublication = usePublish(actualAgent.video("video"), videoSource);
const audioPublication = usePublish(actualAgent.audio("audio"), audioSource, {
  enabled: true,
});
const dataPublication = usePublish(actualAgent.data("data"));
const remote = actualAgent.room("room").participant("alice");
const videoSubscription = useSubscribe(remote.video("video"));
const audioSubscription = useSubscribe(remote.audio("audio"));
const dataSubscription = useSubscribe(remote.data("data"));
type VideoPublicationIdentity = Expect<
  Equal<typeof videoPublication, VideoPublication | null>
>;
type AudioPublicationIdentity = Expect<
  Equal<typeof audioPublication, AudioPublication | null>
>;
type DataPublicationIdentity = Expect<
  Equal<typeof dataPublication, DataPublication | null>
>;
type VideoSubscriptionIdentity = Expect<
  Equal<typeof videoSubscription, VideoSubscription | null>
>;
type AudioSubscriptionIdentity = Expect<
  Equal<typeof audioSubscription, AudioSubscription | null>
>;
type DataSubscriptionIdentity = Expect<
  Equal<typeof dataSubscription, DataSubscription | null>
>;
usePublish(actualAgent.video("video"), null);
usePublish(null, null);
useSubscribe(null);
// @ts-expect-error Audio sources cannot feed video publications.
usePublish(actualAgent.video("video"), audioSource);
// @ts-expect-error A remote track cannot be published.
usePublish(remote.video("video"), videoSource);
// @ts-expect-error A local track cannot be subscribed to.
useSubscribe(actualAgent.video("video"));
void [
  videoPublication,
  audioPublication,
  dataPublication,
  videoSubscription,
  audioSubscription,
  dataSubscription,
];
const direct = new WebAgent(config);
direct.join("explicit-room");
await direct.close();
// @ts-expect-error Required ownership configuration.
useAgent();

const devices = useDevices();
const cameraDevice: CameraDevice | undefined = devices.cameras[0];
const microphone: MicrophoneDevice | undefined = devices.microphones[0];
// @ts-expect-error Speaker inventory is outside capture.
devices.speakers;
const capture = useCapture("device", {
  video: {
    device: cameraDevice,
    width: 1280,
    height: 720,
    frameRate: 30,
  },
  audio: { device: microphone, echoCancellation: true },
});
useCapture("device", { video: true, audio: true });
useCapture("display", { video: { height: 1080 }, audio: true });
// @ts-expect-error Browser constraints are not portable options.
useCapture("device", { video: { width: { exact: 1280 } }, audio: false });
// @ts-expect-error Device kinds are distinct.
useCapture("device", { video: { device: microphone }, audio: false });
// @ts-expect-error Display capture has no exact device selection.
useCapture("display", { video: { device: cameraDevice }, audio: false });
// @ts-expect-error Display video is required.
useCapture("display", { video: false, audio: false });

// @ts-expect-error Descriptors are not normal SDK API.
import { track } from "@pulsebeam/react";
// @ts-expect-error Descriptor-only types are not React API.
import type { TrackSpec } from "@pulsebeam/react";
// @ts-expect-error Descriptor-only types are not React API.
import type { VideoTrackSpec } from "@pulsebeam/react";
// @ts-expect-error Descriptor-only types are not React API.
import type { AudioTrackSpec } from "@pulsebeam/react";
// @ts-expect-error Descriptor-only types are not React API.
import type { DataTrackSpec } from "@pulsebeam/react";

const localVideo = agent.video("camera");
const localAudio = agent.audio("microphone");
const chat = agent.data("chat");
usePublish(localVideo, capture.video, { enabled: false })?.configure({
  enabled: true,
});
usePublish(localAudio, capture.audio);
// @ts-expect-error Audio source cannot bind a video publication.
usePublish(localVideo, capture.audio);
// @ts-expect-error Browser sender-specific options are not publication options.
videoPublication?.configureSender({ contentHint: "speech" });
// @ts-expect-error No normal imperative publication setter.
localVideo.setSource(capture.video);
// @ts-expect-error Publication enabled state belongs to Publish.
localVideo.setEnabled(false);
// @ts-expect-error Local handles do not expose borrowed capture.
localVideo.source;
// @ts-expect-error Descriptors are not React lookup API.
agent.track({ kind: "video", trackId: "camera" });
// @ts-expect-error Delivery belongs on the owner.
agent.data("chat", "ordered");

const participant = agent.room("selected-room").participant("alice");
const participantId: string = participant.id();
void participantId;
const remoteVideo = participant.video("camera");
const remoteAudio = participant.audio("microphone");
const remoteChat = participant.data("chat");
// @ts-expect-error Participant descriptor lookup was removed.
participant.track({ kind: "video", trackId: "camera" });
// @ts-expect-error Delivery belongs on the owner.
participant.data("chat", "ordered");
Data({
  track: participant.data("live"),
  options: { delivery: DataDelivery.Unreliable },
  async onEvent(event) {
    if (!DataEvent.Message.instanceOf(event)) return;
    const bytes: Uint8Array = event.inner.data;
    // @ts-expect-error Messages do not carry gap bounds.
    event.inner.from;
    await Promise.resolve(bytes);
  },
});
Data({
  track: participant.data("state"),
  options: { delivery: DataDelivery.Latest },
  onEvent(event) {
    if (DataEvent.Discontinuity.instanceOf(event))
      void event.inner.previousPublicationId;
  },
});
createElement(Video, {
  source: useSubscribe(remoteVideo, {
    visible: true,
    height: 360,
    minHeight: 360,
    minFps: 15,
    priority: 1,
  }),
});
// @ts-expect-error Playback cannot create an implicit reception owner.
Video({ source: remoteVideo });
// @ts-expect-error Reception configuration belongs to the independent subscription.
Video({ source: videoSubscription, receive: {} });
// @ts-expect-error Audio subscriptions cannot feed video playback.
Video({ source: audioSubscription });
// @ts-expect-error Playback does not select or own agent-wide audio reception.
Audio({ agent });
createElement(Video, { source: capture.video });
createElement(Audio, {
  source: useSubscribe(remoteAudio),
  onPlaybackError: (failure) => void failure.retry(),
});
createElement(Data, {
  track: remoteChat,
  onEvent(event) {
    const participantId: string = event.inner.participantId;
    const publicationId: string = event.inner.publicationId;
    if (DataEvent.Message.instanceOf(event)) {
      const bytes: Uint8Array = event.inner.data;
      const sequence: bigint = event.inner.sequence;
      void [bytes, sequence];
    } else if (DataEvent.Discontinuity.instanceOf(event)) {
      const previous: string = event.inner.previousPublicationId;
      void previous;
    }
    void [participantId, publicationId];
  },
});
// @ts-expect-error Local publications cannot be received by the same endpoint.
createElement(Data, { track: chat, onEvent: () => {} });
// @ts-expect-error Sending requires an owned publication.
void chat.send(new Uint8Array([0, 255]));
const chatPublication = usePublish(chat);
if (chatPublication) void chatPublication.send(new Uint8Array([0, 255]));
// @ts-expect-error React data handles own receive only through Data.
chat.events();
// @ts-expect-error React remote data handles own receive only through Data.
remoteChat.events();
// @ts-expect-error Remote data is receive-only.
remoteChat.send(new Uint8Array());
// @ts-expect-error Remote receive policy belongs to Video.
remoteVideo.setReceiveOptions({});
// @ts-expect-error Audio analysis is not per-track React API.
remoteAudio.readWaveform(new Float32Array());
// @ts-expect-error Nested remote lookup roots were removed.
agent.remote;
// @ts-expect-error Kind-specific participant collection roots were removed.
participant.audioTracks;
// @ts-expect-error Old callable local roots were removed.
agent.local;
// @ts-expect-error Typed JSON topics were removed.
agent.topic("chat", { mode: "ordered" });
// @ts-expect-error Retry is join(room), not a second session operation.
agent.reconnect();
// The returned handle is the SDK Agent, including its terminal close operation.
agent.close();
// @ts-expect-error No low-level normal observation.
agent.getSnapshot();

const videoSettings: VideoCaptureSettings | undefined = capture.video?.settings;
const audioSettings: AudioCaptureSettings | undefined = capture.audio?.settings;
void [videoSettings, audioSettings, remoteAudio];
// @ts-expect-error Discovery belongs to bounded Watch pages, not pinned references.
participant.tracks;
// @ts-expect-error Flat publication collection aliases are not re-exported by React.
import type { RemotePublishedTrack } from "../../src/index.js";
// @ts-expect-error Only join requests admission.
agent.connect();
// @ts-expect-error Close is the explicit session shutdown operation.
agent.disconnect();
