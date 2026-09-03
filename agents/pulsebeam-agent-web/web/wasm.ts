import initializeWasm, {
  BrowserRuntime as WasmRuntime,
  configure_logging as configureWasmLogging,
} from "../dist/wasm/pulsebeam_agent_web.js";

import type { LogLevel, LogSink } from "./types.js";

export type RuntimeHost = WasmRuntime;

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
