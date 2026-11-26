import {
  Agent,
  AgentStatus,
  AudioCollectionRef,
  AudioTrack,
  Catalog,
  CollectionStatus,
  DataDelivery,
  DataEvent,
  DataPublication,
  DataSetRef,
  RoomRef,
  VideoCollectionRef,
  VideoTrack,
  WatchStatus,
  type AudioCollection,
  type DataSubscription,
  type VideoCollection,
  type WebAudioOutput,
  type WebVideoOutput,
} from "@pulsebeam/web";
import { Track } from "../../../agents/react/tests/component/platform";
import {
  ScriptedReads,
  scriptedState,
} from "../../../agents/react/tests/component/scripted.js";

// Script admission and the physical playback boundary, using real SDK owners.
type Observed = Agent | Catalog | VideoCollection | AudioCollection;
function overrideState<T extends Observed>(owner: T) {
  const observation = scriptedState(owner.state());
  owner.state = observation.state as T["state"];
  owner.updates = observation.updates as unknown as T["updates"];
  return observation;
}

export function scriptSdk() {
  const agents: Agent[] = [];
  const joins: string[] = [];
  const state = new Map<Agent, ReturnType<typeof overrideState<Agent>>>();
  const watches = new Map<Catalog, ReturnType<typeof overrideState<Catalog>>>();
  const video = new Map<
    VideoCollection,
    ReturnType<typeof overrideState<VideoCollection>>
  >();
  const audio = new Map<
    AudioCollection,
    ReturnType<typeof overrideState<AudioCollection>>
  >();
  const dataSelections = new WeakMap<DataSetRef, string>();
  const data = new Map<
    DataSubscription,
    { trackId: string; reads: ScriptedReads<DataEvent> }
  >();
  const attachments = new Map<string, number>();

  function borrow(kind: "video", publicationId: string): VideoTrack;
  function borrow(kind: "audio", publicationId: string): AudioTrack;
  function borrow(
    kind: "video" | "audio",
    publicationId: string,
  ): VideoTrack | AudioTrack {
    const track: VideoTrack | AudioTrack = Object.create(
      kind === "video" ? VideoTrack.prototype : AudioTrack.prototype,
    );
    track.playbackOutput = (async () => {
      attachments.set(publicationId, (attachments.get(publicationId) ?? 0) + 1);
      const media = new Track(kind);
      return {
        track: () => media as unknown as MediaStreamTrack,
        close: async () => {
          attachments.set(publicationId, attachments.get(publicationId)! - 1);
          media.stop();
        },
      } as WebAudioOutput | WebVideoOutput;
    }) as typeof track.playbackOutput;
    track.sameAdmission = (other: VideoTrack & AudioTrack) => track === other;
    return track;
  }
  const sent: {
    name: string;
    mode: ReturnType<DataPublication["state"]>["delivery"];
    payload: Uint8Array;
  }[] = [];
  let sendCompletion = Promise.resolve();

  const agentState = Agent.prototype.state;
  const agentClose = Agent.prototype.close;
  Agent.prototype.state = function () {
    if (!state.has(this)) {
      agents.push(this);
      this.state = agentState.bind(this);
      state.set(this, overrideState(this));
    }
    return this.state();
  };
  Agent.prototype.join = function (room) {
    joins.push(room);
    state.get(this)!.change({ desiredRoom: room, status: AgentStatus.Joining });
  };
  Agent.prototype.close = async function (...args) {
    await agentClose.call(this, ...args);
    state.get(this)?.change({ status: AgentStatus.Closed });
  };

  const watch = RoomRef.prototype.watch;
  RoomRef.prototype.watch = function (...args) {
    const owner = watch.call(this, ...args);
    const scope = overrideState(owner);
    const close = owner.close.bind(owner);
    owner.close = async (...args) => {
      scope.change({
        status: WatchStatus.Closed,
        participants: undefined,
        hasMore: undefined,
        nextCursor: undefined,
        cursorExpiresAtUnixSeconds: undefined,
      });
      await close(...args);
    };
    watches.set(owner, scope);
    return owner;
  };
  const videoSubscribe = VideoCollectionRef.prototype.subscribe;
  VideoCollectionRef.prototype.subscribe = function () {
    const owner = videoSubscribe.call(this);
    video.set(owner, overrideState(owner));
    return owner;
  };
  const audioSubscribe = AudioCollectionRef.prototype.subscribe;
  AudioCollectionRef.prototype.subscribe = function () {
    const owner = audioSubscribe.call(this);
    audio.set(owner, overrideState(owner));
    return owner;
  };
  const dataRef = RoomRef.prototype.data;
  RoomRef.prototype.data = function (filter) {
    const selection = dataRef.call(this, filter);
    dataSelections.set(selection, filter?.trackId ?? "");
    return selection;
  };
  const dataSubscribe = DataSetRef.prototype.subscribe;
  DataSetRef.prototype.subscribe = function () {
    const owner = dataSubscribe.call(this);
    const reads = new ScriptedReads<DataEvent>();
    owner.next = reads.next;
    const close = owner.close.bind(owner);
    owner.close = async () => {
      reads.close();
      await close();
    };
    data.set(owner, { trackId: dataSelections.get(this)!, reads });
    return owner;
  };
  const dataOwners = () => [...data].filter(([owner]) => !owner.state().closed);
  DataPublication.prototype.send = async function (payload) {
    await sendCompletion;
    sent.push({
      name: this.state().trackId,
      mode: this.state().delivery,
      payload,
    });
  };

  return {
    agents,
    joins,
    changeAgent: (patch: Partial<ReturnType<Agent["state"]>>) =>
      state.get(agents.at(-1)!)!.change(patch),
    watches: () =>
      [...watches.values()].filter(
        (scope) => scope.state().status !== WatchStatus.Closed,
      ),
    mediaOwners: () => [...video.keys(), ...audio.keys()],
    changeMedia() {
      const videoState = [...video.values()].at(-1)!;
      const audioState = [...audio.values()].at(-1)!;
      videoState.change({
        status: CollectionStatus.Live,
        tracks: [
          {
            participantId: "admitted-alice",
            trackId: "camera",
            publicationId: "self-video",
            track: borrow("video", "self-video"),
          },
          {
            participantId: "bob",
            trackId: "camera",
            publicationId: "remote-camera",
            track: borrow("video", "remote-camera"),
          },
          {
            participantId: "bob",
            trackId: "screen",
            publicationId: "remote-screen",
            track: borrow("video", "remote-screen"),
          },
        ],
      });
      audioState.change({
        status: CollectionStatus.Live,
        tracks: [
          {
            participantId: "admitted-alice",
            trackId: "microphone",
            publicationId: "self-audio",
            track: borrow("audio", "self-audio"),
          },
          {
            participantId: "bob",
            trackId: "microphone",
            publicationId: "remote-audio",
            track: borrow("audio", "remote-audio"),
          },
        ],
      });
    },
    attachments,
    dataOwners,
    deliver(
      trackId: string,
      event: Omit<
        Parameters<typeof DataEvent.Message.new>[0],
        "trackId" | "delivery" | "capturedAt" | "fragmentDescriptors"
      >,
      delivery = trackId === "chat"
        ? DataDelivery.Reliable
        : DataDelivery.Unreliable,
    ) {
      for (const [, source] of dataOwners())
        if (source.trackId === trackId)
          source.reads.push(
            DataEvent.Message.new({ trackId, delivery, ...event }),
          );
    },
    sent,
    setSendCompletion: (completion: Promise<void>) => {
      sendCompletion = completion;
    },
  };
}
