import {
  Agent,
  AgentStatus,
  Catalog,
  DataDelivery,
  DataEvent,
  PlayoutDelay,
  VideoSource,
  WatchStatus,
  type AgentConfig,
  type DataSendOptions,
  type VideoObservation,
} from "@pulsebeam/web";

const config: AgentConfig = {
  endpoint: "https://media.example",
  token: "opaque",
};
const agent = new Agent(config);
const joined: AgentStatus = agent.state().status;
const observationRevision: bigint = agent.state().observationRevision;
const admittedParticipant: string | undefined = agent.state().participantId;
agent.join("demo");
agent.renew("renewed");

const room = agent.room("demo");
const watch: Catalog = room.watch({ limit: 10, cursor: new Uint8Array() });
const page = watch.state();
const participant = page.participants?.[0];
const participants =
  watch.state().status === WatchStatus.Live ? page.participants : undefined;
const recipient = participant?.dataRecipientId();
const cursor: Uint8Array | undefined = page.nextCursor;
const local = agent.video("camera");
declare const media: MediaStreamTrack;
const source = VideoSource.fromTrack(media);
const publication = local.publish({ enabled: true });
const input = source.pipeTo(publication);
publication.configure({ enabled: false });
const remote = room.participant("peer");
const videoOptions: VideoObservation = {
  visible: true,
  height: 360,
  minHeight: 0,
  minFps: 15,
  priority: 10,
  playoutDelay: PlayoutDelay.Fixed.new({ minMs: 100, maxMs: 200 }),
};
const reception = remote.video("camera").subscribe(videoOptions);
const output = await reception.playbackOutput();
const received: MediaStreamTrack | undefined = output.track();
const data = agent.data("data").publish({ delivery: DataDelivery.Reliable });
const sends: DataSendOptions | undefined = recipient
  ? { recipients: [recipient] }
  : undefined;
await data.send(Uint8Array.of(0, 255), sends);
const messages = remote.data("data").subscribe();
const event = await messages.next();
if (event && DataEvent.Message.instanceOf(event)) {
  const bytes: Uint8Array = event.inner.data;
  const sequence: bigint = event.inner.sequence;
  void [bytes, sequence];
}
const updates = agent.updates();
await updates.next();
updates.close();
await Promise.all([
  input.close(),
  output.close(),
  reception.close(),
  publication.close(),
  watch.close(),
  source.close(),
  messages.close(),
  data.close(),
]);
await agent.close();
void [
  joined,
  observationRevision,
  admittedParticipant,
  participants,
  cursor,
  received,
];

// @ts-expect-error No handwritten factory.
import { createAgent } from "@pulsebeam/web";
// @ts-expect-error Low-level transport runtime is not public.
import { BrowserRuntime } from "@pulsebeam/web";
// @ts-expect-error Removed descriptor factory.
import { track } from "@pulsebeam/web";
// @ts-expect-error No handwritten Watch alias.
import type { Watch } from "@pulsebeam/web";
// @ts-expect-error No implicit reconnect without explicit join intent.
agent.reconnect();
// @ts-expect-error No unpinned participant lookup.
agent.participant("peer");
// @ts-expect-error Use state().status.
agent.connection;
// @ts-expect-error Session config has no room.
new Agent({ ...config, roomId: "demo" });
// @ts-expect-error Catalog metadata is only available through watch ownership.
agent.state().catalog;
// @ts-expect-error Physical receiver mapping is private.
agent.state().mapping;
// @ts-expect-error Publish owns input association through a separate pipe.
local.publish(source);
// @ts-expect-error Source replacement belongs to pipes.
publication.setSource(source);
// @ts-expect-error Browser sender settings are not publication options.
publication.configureSender({ contentHint: "text" });
// @ts-expect-error Playback output acquisition belongs to an owned subscription.
remote.video("camera").playbackOutput();
// @ts-expect-error Remote endpoints cannot publish.
remote.data("data").publish();
// @ts-expect-error Local endpoints cannot subscribe.
agent.data("data").subscribe();
// @ts-expect-error Data must be bytes, not an implicit JSON codec.
await data.send({ test: true });

// @ts-expect-error Catalog metadata and continuation are fields on the snapshot itself.
watch.state().page;
