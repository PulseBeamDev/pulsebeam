// Only the external WASM transport boundary is modeled. Facade, topic,
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
  tracks = new Map<number, MediaStreamTrack>();
  replacements: { slot: string; track: MediaStreamTrack | null }[] = [];
  replaceCompletion: Promise<void> = Promise.resolve();
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
  }
  renew_authorization(token: string) {
    this.config = { ...this.config, token };
  }
  set_log_level() {}
  async set_local_muted() {}
  async replace_local_track(slot: string, track: MediaStreamTrack | null) {
    await this.replaceCompletion;
    this.replacements.push({ slot, track });
  }
  remote_track(index: number) {
    return this.tracks.get(index);
  }
  async send_topic(
    name: string,
    mode: string,
    payload: Uint8Array,
    _generation: bigint,
    _dataTrackId: string,
  ) {
    this.sent.push({ name, mode, payload });
  }
  emit(patch: Record<string, unknown>) {
    this.snapshot = {
      ...this.snapshot,
      ...patch,
      version: this.snapshot.version + 1n,
    };
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
