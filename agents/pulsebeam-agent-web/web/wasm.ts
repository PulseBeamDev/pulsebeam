import initializeWasm, {
  BrowserRuntime,
} from "../dist/wasm/pulsebeam_agent_web.js";

const initialization = initializeWasm().then((wasm) => {
  return wasm;
});

void initialization.catch(() => {});

export { BrowserRuntime };

export function whenInitialized(): Promise<void> {
  return initialization.then(() => undefined);
}
