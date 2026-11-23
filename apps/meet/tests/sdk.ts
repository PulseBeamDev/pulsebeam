import {
  Agent,
  AgentStatus,
  AudioSubscription,
  Catalog,
  DataEvent,
  DataPublication,
  RemoteAudioTrack,
  RemoteVideoTrack,
  RoomRef,
  VideoSubscription,
  WatchStatus,
} from "@pulsebeam/web";
import {
  captureDataReads,
  scriptedState,
} from "../../../agents/react/tests/component/scripted.js";

// Use real generated owners. Script only the values that would come from
// an admitted SFU connection, so this component test never opens a transport.
function overrideState<T extends Agent | Catalog>(owner: T) {
  const observation = scriptedState(owner.state());
  owner.state = observation.state as T["state"];
  owner.updates = observation.updates as unknown as T["updates"];
  return observation;
}
function addOwner<T extends { state(): { closed: boolean } }>(
  owners: T[],
  owner: T,
) {
  owners.push(owner);
  return owner;
}

export function scriptSdk() {
  const agents: Agent[] = [];
  const joins: string[] = [];
  const state = new Map<Agent, ReturnType<typeof overrideState<Agent>>>();
  const watches = new Map<Catalog, ReturnType<typeof overrideState<Catalog>>>();
  const video: VideoSubscription[] = [];
  const audio: AudioSubscription[] = [];
  const data = captureDataReads();
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
  const videoSubscribe = RemoteVideoTrack.prototype.subscribe;
  RemoteVideoTrack.prototype.subscribe = function (...args) {
    return addOwner(video, videoSubscribe.call(this, ...args));
  };
  const audioSubscribe = RemoteAudioTrack.prototype.subscribe;
  RemoteAudioTrack.prototype.subscribe = function (...args) {
    return addOwner(audio, audioSubscribe.call(this, ...args));
  };
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
    videos: () =>
      video
        .filter((owner) => !owner.state().closed)
        .map((owner) => owner.state()),
    audios: () =>
      audio
        .filter((owner) => !owner.state().closed)
        .map((owner) => owner.state()),
    dataOwners: data.owners,
    deliver(
      trackId: string,
      event: Omit<Parameters<typeof DataEvent.Message.new>[0], "capturedAt">,
    ) {
      data.deliver(trackId, DataEvent.Message.new(event));
    },
    sent,
    setSendCompletion: (completion: Promise<void>) => {
      sendCompletion = completion;
    },
  };
}
