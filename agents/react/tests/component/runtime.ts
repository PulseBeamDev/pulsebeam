// Only the external WASM transport boundary is modeled. Facade, data,
// resource, capture and component policy are the production implementations.
export const runtimes: BrowserRuntime[] = [];
export const whenInitialized = async () => {};

export class BrowserRuntime {
  closed = false;
  config: any;
  desired: any;
  snapshotListener?: (snapshot: any) => void;
  eventListener?: (event: any) => void;
  errorListener?: (error: any) => void;
  sent: { name: string; mode: string; payload: Uint8Array }[] = [];
  sendCompletion: Promise<void> = Promise.resolve();
  tracks = new Map<number, MediaStreamTrack>();
  replacements: { slot: string; track: MediaStreamTrack | null }[] = [];
  replaceCompletion: Promise<void> = Promise.resolve();
  nextOwner = 1n;
  videoOwners = new Map<bigint, { selector: any; observation?: any }>();
  videoSlots = new Map<string, number>();
  publicationOwners = new Map<bigint, { key: string; slot: string }>();
  claimedPublications = new Set<string>();
  resourceCommands: any[] = [];
  ownedReplacements: {
    owner: bigint;
    track: MediaStreamTrack | null;
    config: any;
  }[] = [];
  ownedMuted = new Map<bigint, boolean>();
  audioOwner?: bigint;
  dataOwners = new Map<
    bigint,
    { spec: { label: string; delivery: string }; events: any[] }
  >();
  snapshot = {
    version: 1n,
    desiredRevision: 0n,
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
      const key = `${command.spec.kind}:${command.spec.label}`;
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
    if (command.op === "receiveData") {
      const owner = this.nextOwner++;
      this.dataOwners.set(owner, { spec: command.spec, events: [] });
      return { owner };
    }
    if (command.op === "readData")
      return this.dataOwners.get(command.owner)?.events.shift() ?? null;
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
      this.videoOwners.delete(command.owner);
      const publication = this.publicationOwners.get(command.owner);
      if (publication) this.claimedPublications.delete(publication.key);
      this.publicationOwners.delete(command.owner);
      this.dataOwners.delete(command.owner);
      if (this.audioOwner === command.owner) this.audioOwner = undefined;
      this.recomputeVideo();
      return { type: "released" };
    }
    if (command.op === "updatePublication") return { type: "updated" };
    throw new Error(`unsupported resource command ${command.op}`);
  }
  recomputeVideo() {
    if (!this.desired) return;
    const grouped = new Map<string, { selector: any; views: any[] }>();
    for (const { selector, observation } of this.videoOwners.values()) {
      if (!observation?.visible || observation.height <= 0) continue;
      const key = `${selector.participantId}\u0000${selector.label}`;
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
          item.label === selector.label,
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
        view.playoutDelay ? [view.playoutDelay] : [],
      );
      video.push({
        slot,
        trackId: publication.id,
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
  renew_authorization(token: string) {
    this.config = { ...this.config, token };
  }
  set_log_level() {}
  async set_local_muted() {}
  async replace_owned_track(
    owner: bigint,
    track: MediaStreamTrack | null,
    config: any,
  ) {
    this.ownedReplacements.push({ owner, track, config });
  }
  async set_owned_muted(owner: bigint, muted: boolean) {
    this.ownedMuted.set(owner, muted);
  }
  async replace_local_track(slot: string, track: MediaStreamTrack | null) {
    await this.replaceCompletion;
    this.replacements.push({ slot, track });
  }
  remote_track(index: number) {
    return this.tracks.get(index);
  }
  async send_data(
    spec: { label: string; delivery: string },
    payload: Uint8Array,
  ) {
    await this.sendCompletion;
    this.sent.push({ name: spec.label, mode: spec.delivery, payload });
  }
  deliverData(label: string, event: unknown) {
    const owners: bigint[] = [];
    for (const [owner, receiver] of this.dataOwners) {
      if (receiver.spec.label !== label) continue;
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
  }
  event(event: unknown) {
    this.eventListener?.(event);
  }
  close() {
    this.closed = true;
  }
  abort() {
    this.closed = true;
  }
}
