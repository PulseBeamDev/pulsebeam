// Only the external WASM transport boundary is modeled. Facade, data,
// resource, capture and component policy are the production implementations.
import { PipeStatus, WatchStatus } from "@pulsebeam/web";

export const runtimes: BrowserRuntime[] = [];
export const initializeSynchronously = () => {};

export class BrowserRuntime {
  closed = false;
  config: any;
  desired: any = { video: [] };
  snapshotListener?: (snapshot: any) => void;
  eventListener?: (event: any) => void;
  errorListener?: (error: any) => void;
  sent: { name: string; mode: string; payload: Uint8Array }[] = [];
  joins: string[] = [];
  sendCompletion: Promise<void> = Promise.resolve();
  tracks = new Map<number, MediaStreamTrack>();
  nextOwner = 1n;
  catalogWatches = new Map<
    bigint,
    { room: string; limit: number; cursor: readonly number[]; closed: boolean }
  >();
  videoOwners = new Map<bigint, { selector: any; observation?: any }>();
  videoSlots = new Map<string, number>();
  publicationOwners = new Map<bigint, { key: string; slot: string }>();
  claimedPublications = new Set<string>();
  resourceCommands: any[] = [];
  ownedReplacements: {
    owner: bigint;
    track: MediaStreamTrack | null;
    config: any;
    muted: boolean;
    replaceInput: boolean;
  }[] = [];
  ownedMuted = new Map<bigint, boolean>();
  audioOwner?: bigint;
  audioSelections = new Map<bigint, any>();
  audioSubscriptions = new Map<bigint, any>();
  dataPublications = new Map<bigint, any>();
  observationWaiters = new Map<bigint, Set<(error: unknown) => void>>();
  dataOwners = new Map<
    bigint,
    { spec: { trackId: string; delivery: string }; events: any[] }
  >();
  snapshot = {
    version: 1n,
    desiredRevision: 0n,
    joinRevision: 0n,
    credentialRevision: 0n,
    acceptedCredentialRevision: undefined,
    credentialOutcome: undefined,
    desiredRoom: undefined as string | undefined,
    connection: "disconnected",
    catalog: { revision: 0n, tracks: [] },
    mapping: { acceptedIntentRevision: 0n, video: [], audio: [] },
    participants: [],
    publications: [],
    topics: {
      publishers: [],
      subscribers: [],
      acceptedSends: 0n,
      droppedSends: 0n,
      deliveredMessages: 0n,
      channelFailures: 0n,
    },
  };
  constructor(config: unknown) {
    this.config = config;
    runtimes.push(this);
  }
  readonly observations = new Map<
    object | bigint | "agent",
    { state: any; changed: Set<() => void> }
  >();
  observer(owner: object | bigint | "agent" = "agent") {
    let entry = this.observations.get(owner);
    if (!entry) {
      entry = {
        state: Object.freeze({ observationRevision: 1n }),
        changed: new Set(),
      };
      this.observations.set(owner, entry);
    }
    const current = entry;
    return {
      state: () => current.state,
      updates() {
        let previous: unknown;
        let resolve: ((value: any) => void) | undefined;
        let closed = false;
        const changed = () => {
          if (resolve) {
            const done = resolve;
            resolve = undefined;
            previous = current.state;
            done(previous);
          }
        };
        current.changed.add(changed);
        return {
          next: () => {
            if (closed) return Promise.resolve(undefined);
            if (previous !== current.state) {
              previous = current.state;
              return Promise.resolve(previous);
            }
            return new Promise((done) => {
              resolve = done;
            });
          },
          close: () => {
            closed = true;
            current.changed.delete(changed);
            resolve?.(undefined);
            resolve = undefined;
          },
        };
      },
    };
  }
  setObservation(owner: object | bigint | "agent", state: object) {
    this.observer(owner);
    const entry = this.observations.get(owner)!;
    entry.state = Object.freeze(state);
    for (const changed of entry.changed) changed();
  }
  state() {
    return this.observer().state();
  }
  publication(owner: bigint) {
    if (!this.observations.has(owner))
      this.setObservation(owner, {
        observationRevision: 1n,
        configuration: Object.freeze({}),
        admission: Object.freeze({}),
      });
    return this.observer(owner);
  }
  catalog_watch(room: string, limit: number, cursor: Uint8Array) {
    const owner = this.nextOwner++;
    const scope = {
      room,
      limit,
      cursor: Object.freeze(Array.from(cursor)),
      closed: false,
    };
    this.catalogWatches.set(owner, scope);
    this.setObservation(owner, {
      observationRevision: 1n,
      room,
      limit,
      cursor: scope.cursor,
      status: WatchStatus.Loading,
    });
    return {
      ...this.observer(owner),
      close: () => {
        if (scope.closed) return;
        scope.closed = true;
        const state = this.observer(owner).state() as any;
        this.setObservation(owner, {
          ...state,
          observationRevision: state.observationRevision + 1n,
          status: WatchStatus.Closed,
          page: undefined,
        });
      },
    };
  }
  video_subscription(owner: bigint) {
    return this.observer(owner);
  }
  audio_subscription(owner: bigint) {
    return this.observer(owner);
  }
  data_subscription(owner: bigint) {
    return this.observer(owner);
  }
  data_publication(owner: bigint) {
    return this.observer(owner);
  }
  set_snapshot_listener(listener?: (snapshot: any) => void) {
    this.snapshotListener = listener;
    listener?.(this.snapshot);
  }
  set_event_listener(listener?: (event: any) => void) {
    this.eventListener = listener;
  }
  set_error_listener(listener?: (error: any) => void) {
    this.errorListener = listener;
  }
  replace_desired(desired: unknown) {
    this.desired = desired;
    this.recomputeVideo();
  }
  async resource(command: any) {
    if (
      command.op !== "release" ||
      this.publicationOwners.has(command.owner) ||
      this.videoOwners.has(command.owner) ||
      this.dataOwners.has(command.owner) ||
      this.audioSelections.has(command.owner) ||
      this.audioSubscriptions.has(command.owner) ||
      this.dataPublications.has(command.owner) ||
      this.audioOwner === command.owner
    )
      return this.resource_now(command);
  }
  resource_now(command: any) {
    this.resourceCommands.push(command);
    if (command.op === "claimVideo") {
      const owner = this.nextOwner++;
      this.videoOwners.set(owner, { selector: command.selector });
      return { owner };
    }
    if (command.op === "observeVideo") {
      const entry = this.videoOwners.get(command.owner);
      if (!entry) throw new Error("released video owner");
      entry.observation = command.observation;
      this.recomputeVideo();
      return { type: "updated" };
    }
    if (command.op === "claimPublication") {
      const key = `${command.spec.kind}:${command.spec.trackId}`;
      if (this.claimedPublications.has(key)) {
        const error = new Error("publication already has an owner") as Error & {
          pulsebeamCode?: string;
        };
        error.pulsebeamCode = "ownership";
        throw error;
      }
      const owner = this.nextOwner++;
      const slot = `${command.spec.kind === "video" ? "v" : "a"}0`;
      this.claimedPublications.add(key);
      this.publicationOwners.set(owner, { key, slot });
      return { owner, slot };
    }
    if (command.op === "declareDataPublication") {
      const owner = this.nextOwner++;
      this.dataPublications.set(owner, command.spec);
      return { owner };
    }
    if (command.op === "claimAudioSubscription") {
      const owner = this.nextOwner++;
      this.audioSubscriptions.set(owner, command.selector);
      return { owner };
    }
    if (command.op === "configureAudio") return { type: "updated" };
    if (command.op === "receiveData") {
      const owner = this.nextOwner++;
      this.dataOwners.set(owner, { spec: command.spec, events: [] });
      return { owner };
    }
    if (command.op === "readData")
      return this.dataOwners.get(command.owner)?.events.shift() ?? null;
    if (command.op === "claimAudioPolicy") {
      const owner = this.nextOwner++;
      this.audioSelections.set(owner, command.policy);
      return { owner };
    }
    if (command.op === "configureAudioPolicy") {
      if (!this.audioSelections.has(command.owner))
        throw new Error("released selection owner");
      this.audioSelections.set(command.owner, command.policy);
      return { type: "updated" };
    }
    if (command.op === "claimAudio") {
      if (this.audioOwner !== undefined) {
        const error = new Error("audio already has an owner") as Error & {
          pulsebeamCode?: string;
        };
        error.pulsebeamCode = "ownership";
        throw error;
      }
      const owner = this.nextOwner++;
      this.audioOwner = owner;
      return { owner };
    }
    if (command.op === "release") {
      this.audioSubscriptions.delete(command.owner);
      this.dataPublications.delete(command.owner);
      this.videoOwners.delete(command.owner);
      this.retireObservations(command.owner);
      const publication = this.publicationOwners.get(command.owner);
      if (publication) this.claimedPublications.delete(publication.key);
      this.publicationOwners.delete(command.owner);
      this.dataOwners.delete(command.owner);
      this.audioSelections.delete(command.owner);
      if (this.audioOwner === command.owner) this.audioOwner = undefined;
      this.recomputeVideo();
      return { type: "released" };
    }
    if (command.op === "updatePublication") return { type: "updated" };
    throw new Error(`unsupported resource command ${command.op}`);
  }
  // This component boundary has no physical receiver or decoded DOM media.
  async observe_media(
    owner: bigint,
    _source?: string,
    signal?: AbortSignal,
  ): Promise<any> {
    if (
      signal?.aborted ||
      this.closed ||
      (!this.videoOwners.has(owner) && !this.audioSubscriptions.has(owner))
    )
      throw Object.assign(new Error("observation owner closed"), {
        pulsebeamCode: "closed",
      });
    return new Promise((_resolve, reject) => {
      const waiters = this.observationWaiters.get(owner) ?? new Set();
      const cancel = () =>
        finish(
          Object.assign(new Error("observation cancelled"), {
            pulsebeamCode: "closed",
          }),
        );
      const finish = (error: unknown) => {
        signal?.removeEventListener("abort", cancel);
        waiters.delete(finish);
        if (waiters.size === 0) this.observationWaiters.delete(owner);
        reject(error);
      };
      waiters.add(finish);
      this.observationWaiters.set(owner, waiters);
      signal?.addEventListener("abort", cancel, { once: true });
    });
  }
  retireObservations(owner?: bigint) {
    for (const [token, waiters] of this.observationWaiters) {
      if (owner !== undefined && token !== owner) continue;
      this.observationWaiters.delete(token);
      for (const reject of waiters)
        reject(
          Object.assign(new Error("observation owner closed"), {
            pulsebeamCode: "closed",
          }),
        );
    }
  }
  recomputeVideo() {
    if (!this.desired) return;
    const grouped = new Map<string, { selector: any; views: any[] }>();
    for (const { selector, observation } of this.videoOwners.values()) {
      if (!observation?.visible || observation.height <= 0) continue;
      const key = `${selector.participantId}\u0000${selector.trackId}`;
      const entry = grouped.get(key) ?? { selector, views: [] };
      entry.views.push(observation);
      grouped.set(key, entry);
    }
    const video = [];
    const occupied = new Set<number>();
    for (const [key, { selector, views }] of grouped) {
      const publication = this.snapshot.publications.find(
        (item: any) =>
          item.kind === "video" &&
          item.participantId === selector.participantId &&
          item.trackId === selector.trackId,
      );
      if (!publication) continue;
      let slot = this.videoSlots.get(key);
      if (slot === undefined || occupied.has(slot)) {
        slot = 0;
        while (occupied.has(slot)) slot++;
        this.videoSlots.set(key, slot);
      }
      occupied.add(slot);
      const delays = views.flatMap((view) =>
        view.playoutDelay?.mode === "fixed" ? [view.playoutDelay] : [],
      );
      video.push({
        slot,
        publicationId: publication.id,
        height: Math.max(...views.map((view) => view.height)),
        minHeight: Math.max(...views.map((view) => view.minHeight)),
        minFps: Math.max(...views.map((view) => view.minFps)),
        priority: Math.max(...views.map((view) => view.priority)),
        ...(delays.length
          ? {
              playoutDelay: {
                mode: "fixed",
                minMs: Math.min(...delays.map((delay) => delay.minMs)),
                maxMs: Math.max(...delays.map((delay) => delay.maxMs)),
              },
            }
          : {}),
      });
    }
    this.desired = { ...this.desired, video };
  }
  join(room: string) {
    this.joins.push(room);
  }
  renew(token: string) {
    this.config = { ...this.config, token };
  }
  set_log_level() {}
  configure_publication(owner: bigint, enabled: boolean) {
    this.ownedMuted.set(owner, !enabled);
  }
  create_owned_publication_pipe(owner: bigint) {
    const key = {};
    const observation = this.observer(key);
    this.setObservation(key, {
      observationRevision: 1n,
      status: PipeStatus.Available,
    });
    return {
      ...observation,
      install_track: (track: MediaStreamTrack, configuration: any) =>
        this.replace_owned_track(
          owner,
          track,
          configuration,
          this.ownedMuted.get(owner) ?? false,
          true,
        ),
      close: () => {
        this.setObservation(key, {
          observationRevision: observation.state().observationRevision + 1n,
          status: PipeStatus.Closed,
        });
        return this.replace_owned_track(owner, null, {}, true, true);
      },
    };
  }
  async replace_owned_track(
    owner: bigint,
    track: MediaStreamTrack | null,
    config: any,
    muted: boolean,
    replaceInput: boolean,
  ) {
    this.ownedReplacements.push({ owner, track, config, muted, replaceInput });
    this.ownedMuted.set(owner, muted);
  }
  async set_owned_muted(owner: bigint, muted: boolean) {
    this.ownedMuted.set(owner, muted);
  }
  remote_track(index: number) {
    return this.tracks.get(index);
  }
  async send_owned_data(owner: bigint, payload: Uint8Array) {
    const spec = this.dataPublications.get(owner);
    if (!spec) throw new Error("released data publication owner");
    await this.sendCompletion;
    this.sent.push({ name: spec.trackId, mode: spec.delivery, payload });
  }
  deliverData(trackId: string, event: unknown) {
    const owners: bigint[] = [];
    for (const [owner, receiver] of this.dataOwners) {
      if (receiver.spec.trackId !== trackId) continue;
      receiver.events.push(event);
      owners.push(owner);
    }
    this.event({ type: "data-ready", owners });
  }
  emit(patch: Record<string, unknown>) {
    this.snapshot = {
      ...this.snapshot,
      ...patch,
      version: this.snapshot.version + 1n,
    };
    this.recomputeVideo();
    this.snapshotListener?.(this.snapshot);
    this.setObservation("agent", {
      observationRevision: this.observer().state().observationRevision + 1n,
    });
  }
  event(event: unknown) {
    this.eventListener?.(event);
  }
  close() {
    this.closed = true;
    this.retireObservations();
  }
  abort() {
    this.closed = true;
    this.retireObservations();
  }
}
