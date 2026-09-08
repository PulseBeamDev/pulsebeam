import initializeWasm, {
  BrowserRuntime,
  configure_logging as configureWasmLogging,
} from "../dist/wasm/pulsebeam_agent_web.js";

const initialization = initializeWasm().then((wasm) => {
  configureWasmLogging("warn");
  return wasm;
});

void initialization.catch(() => {});

export { BrowserRuntime };

export function whenInitialized(): Promise<void> {
  return initialization.then(() => undefined);
}
