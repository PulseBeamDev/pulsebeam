import initializeWasm, {
  configure_logging as configureWasmLogging,
} from "../dist/wasm/pulsebeam_agent_web.js";

const initialization = initializeWasm().then((wasm) => {
  configureWasmLogging("warn");
  return wasm;
});

void initialization.catch(() => {});

export function afterInitialization(callback: () => void): void {
  void initialization.then(callback, callback).catch(() => {});
}
