import initializeWasm, {
  BrowserRuntime as WasmRuntime,
  configure_logging as configureWasmLogging,
} from "../dist/wasm/pulsebeam_agent_web.js";

import type { LogLevel, LogSink } from "./types.js";

export interface RuntimeHost {
  set_snapshot_listener(listener?: (snapshot: unknown) => void): void;
  set_event_listener(listener?: (event: unknown) => void): void;
  set_error_listener(listener?: (message: string) => void): void;
  replace_desired(desired: object): void;
  replace_local_track(
    slot: string,
    track: MediaStreamTrack | null,
    config: object,
  ): Promise<void>;
  set_local_muted(slot: string, muted: boolean): Promise<void>;
  force_reconnect(): void;
  send_topic(name: string, mode: string, payload: Uint8Array): void;
  snapshot(): unknown;
  remote_track(mid: string): MediaStreamTrack | undefined;
  statistics(): Promise<unknown>;
  abort(): void;
  free(): void;
}

let initialization: Promise<unknown> | undefined;

async function initialize(): Promise<void> {
  initialization ??= initializeWasm().then((wasm) => {
    configureWasmLogging("warn");
    return wasm;
  });
  await initialization;
}

export async function createRuntime(config: object): Promise<RuntimeHost> {
  await initialize();
  return new WasmRuntime(config) as unknown as RuntimeHost;
}

export async function configureLogging(level: LogLevel, sink?: LogSink): Promise<void> {
  await initialize();
  configureWasmLogging(level, sink);
}
