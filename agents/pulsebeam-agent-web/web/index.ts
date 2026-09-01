import initializeWasm, {
  BrowserRuntime as WasmRuntime,
  configure_logging as configureWasmLogging,
} from "../dist/wasm/pulsebeam_agent_web.js";

export type TopicMode = "latest" | "ordered";
export type LogLevel = "off" | "error" | "warn" | "info" | "debug" | "trace";

export interface RuntimeTopology {
  localVideo?: readonly string[];
  localAudio?: readonly string[];
  remoteVideo?: number;
  remoteAudio?: number;
}

export interface RuntimeTopic {
  name: string;
  mode: TopicMode;
  publish?: boolean;
  subscribe?: boolean;
  publisherId?: string;
}

export interface RuntimeConfig {
  endpoint: string;
  roomId: string;
  requestHeaders?: Readonly<Record<string, string>>;
  topology: RuntimeTopology;
  topics?: readonly RuntimeTopic[];
}

export interface RuntimeSnapshot {
  version: bigint;
  desiredRevision: bigint;
  connection: string;
  generation?: bigint;
  participantId?: string;
  participants: number;
  publications: number;
  videoBindings: number;
  audioBindings: number;
  terminalFailure?: string;
}

export interface TopicMessageEvent {
  type: "topic-message";
  mode: TopicMode;
  topic: string;
  publisherId?: string;
  streamId?: bigint;
  sequence?: bigint;
  payload: Uint8Array;
}

export interface RuntimeDiagnostics {
  peers: number;
  requests: number;
  timers: number;
  closed: boolean;
  lastError?: string;
}

export type RuntimeEvent = TopicMessageEvent | Readonly<Record<string, unknown>>;
export type LogSink = (level: LogLevel, target: string, message: string) => void;

let initialization: Promise<unknown> | undefined;

function initialize(): Promise<unknown> {
  initialization ??= initializeWasm().then((wasm) => {
    configureWasmLogging("warn");
    return wasm;
  });
  return initialization;
}

export async function createRuntime(config: RuntimeConfig): Promise<BrowserRuntime> {
  await initialize();
  return new BrowserRuntime(new WasmRuntime(normalizeConfig(config)));
}

export async function configureLogging(level: LogLevel, sink?: LogSink): Promise<void> {
  await initialize();
  configureWasmLogging(level, sink);
}

export class BrowserRuntime {
  readonly #runtime: WasmRuntime;
  readonly #snapshotListeners = new Set<() => void>();
  readonly #eventListeners = new Set<(event: RuntimeEvent) => void>();
  readonly #errorListeners = new Set<(message: string) => void>();
  #snapshot: RuntimeSnapshot;
  #closed = false;
  #terminalDiagnostics: RuntimeDiagnostics | undefined;

  constructor(runtime: WasmRuntime) {
    this.#runtime = runtime;
    this.#snapshot = runtime.snapshot() as RuntimeSnapshot;
    runtime.set_snapshot_listener((snapshot: RuntimeSnapshot) => {
      this.#snapshot = snapshot;
      for (const listener of [...this.#snapshotListeners]) listener();
    });
    runtime.set_event_listener((event: RuntimeEvent) => {
      for (const listener of [...this.#eventListeners]) listener(event);
    });
    runtime.set_error_listener((message: string) => {
      for (const listener of [...this.#errorListeners]) listener(message);
    });
  }

  getSnapshot = (): RuntimeSnapshot => this.#snapshot;

  subscribe = (listener: () => void): (() => void) => {
    this.#snapshotListeners.add(listener);
    return () => this.#snapshotListeners.delete(listener);
  };

  onEvent(listener: (event: RuntimeEvent) => void): () => void {
    this.#eventListeners.add(listener);
    return () => this.#eventListeners.delete(listener);
  }

  onError(listener: (message: string) => void): () => void {
    this.#errorListeners.add(listener);
    return () => this.#errorListeners.delete(listener);
  }

  connect(): void {
    if (this.#closed) throw new Error("browser runtime is closed");
    this.#runtime.connect();
  }

  forceReconnect(): void {
    if (this.#closed) throw new Error("browser runtime is closed");
    this.#runtime.force_reconnect();
  }

  sendTopic(name: string, mode: TopicMode, payload: Uint8Array): void {
    if (this.#closed) throw new Error("browser runtime is closed");
    this.#runtime.send_topic(name, mode, payload);
  }

  close(): void {
    if (this.#closed) return;
    this.#runtime.close();
  }

  abort(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#runtime.abort();
    this.#terminalDiagnostics = this.#runtime.diagnostics() as RuntimeDiagnostics;
    queueMicrotask(() => this.#runtime.free());
    this.#snapshotListeners.clear();
    this.#eventListeners.clear();
    this.#errorListeners.clear();
  }

  diagnostics(): RuntimeDiagnostics {
    return this.#terminalDiagnostics ?? (this.#runtime.diagnostics() as RuntimeDiagnostics);
  }
}

function normalizeConfig(config: RuntimeConfig): object {
  return {
    endpoint: config.endpoint,
    roomId: config.roomId,
    requestHeaders: config.requestHeaders ?? {},
    topology: {
      localVideo: [...(config.topology.localVideo ?? [])],
      localAudio: [...(config.topology.localAudio ?? [])],
      remoteVideo: config.topology.remoteVideo ?? 0,
      remoteAudio: config.topology.remoteAudio ?? 0,
    },
    topics: (config.topics ?? []).map((topic) => ({ ...topic })),
  };
}
