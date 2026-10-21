import initializeWasm, {
  BrowserRuntime,
} from "../dist/wasm/pulsebeam_agent_web.js";

let initialization: Promise<void> | undefined;

export { BrowserRuntime };

export function whenInitialized(): Promise<void> {
  return (initialization ??= initializeWasm()
    .then(() => undefined)
    .catch((error: unknown) => {
      initialization = undefined;
      throw error;
    }));
}
