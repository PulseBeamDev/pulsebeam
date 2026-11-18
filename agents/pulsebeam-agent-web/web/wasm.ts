import {
  initSync,
  BrowserPipe,
  BrowserPipeUpdates,
  BrowserRuntime,
} from "../dist/wasm/pulsebeam_agent_web.js";
import { wasmModule } from "./wasm-bytes.js";

let initialized = false;
let initialization: Promise<void> | undefined;

export { BrowserPipe, BrowserPipeUpdates, BrowserRuntime };

export function initializeSynchronously(): void {
  if (initialized) return;
  initSync({
    // Already initialized glue does not need to compile another module.
    get module() {
      return wasmModule();
    },
  });
  initialized = true;
}

export function whenInitialized(): Promise<void> {
  try {
    initializeSynchronously();
    return (initialization ??= Promise.resolve());
  } catch (error: unknown) {
    return Promise.reject(error);
  }
}
