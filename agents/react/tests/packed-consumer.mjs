import assert from "node:assert/strict";
import { cpSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { build } from "esbuild";
import ts from "typescript";

const [directory, react, types, runtime, source] = process.argv
  .slice(2)
  .map((path) => resolve(path));
const installed = new Set();
function install(path) {
  const manifest = JSON.parse(readFileSync(join(path, "package.json")));
  if (installed.has(manifest.name)) return;
  installed.add(manifest.name);
  const destination = join(directory, "node_modules", manifest.name);
  mkdirSync(destination, { recursive: true });
  cpSync(path, destination, { recursive: true, dereference: true });
  const require = createRequire(join(path, "package.json"));
  for (const name of Object.keys(manifest.dependencies ?? {})) {
    const dependency = require.resolve(name + "/package.json");
    install(join(dependency, ".."));
  }
}
for (const path of [react, types, runtime]) install(path);
writeFileSync(join(directory, "package.json"), '{"type":"module"}');
const bundle = join(directory, "consumer.mjs");
await build({
  stdin: {
    contents:
      'export * from "@pulsebeam/react"; export * as web from "@pulsebeam/web";',
    resolveDir: directory,
  },
  outfile: bundle,
  bundle: true,
  platform: "browser",
  format: "esm",
});
const { web, ...sdk } = await import(pathToFileURL(bundle));
for (const name of [
  "useAgent",
  "useDevices",
  "useCapture",
  "usePublish",
  "useSubscribe",
  "Data",
  "Video",
  "Audio",
]) {
  assert.equal(typeof sdk[name], "function", name);
}
assert.equal("Publish" in sdk, false, "publication ownership belongs to hooks");
assert.equal("track" in sdk, false, "React descriptor export was removed");
assert.equal("SelectAudio" in sdk, false, "React collection audio was removed");
assert.equal(
  "createAgent" in sdk,
  false,
  "React has only hook-owned Agent construction",
);
assert.equal(typeof web.Agent, "function");
assert.equal("createAgent" in web, false);
assert.equal("track" in web, false, "Web descriptor export was removed");
assert.equal(
  "attachRemoteVideo" in web,
  false,
  "Web playback requires explicit reception",
);
assert.equal(
  "AudioSelection" in web,
  false,
  "Web collection audio was removed",
);
assert.equal(typeof web.VideoSource.fromTrack, "function");
assert.equal(typeof web.AudioSource.fromTrack, "function");
assert.equal(
  "createManagedAgent" in web,
  false,
  "managed lifecycle is not a public entrypoint",
);
assert.equal("createWebAgent" in web, false);
const agent = new web.Agent({
  endpoint: "http://localhost:7070",
  token: "inert",
});
assert.ok(web.Agent.instanceOf(agent));
assert.equal(
  agent.state(),
  agent.state(),
  "unchanged generated snapshot identity",
);
await agent.close();
const consumer = join(directory, "consumer.ts");
cpSync(source, consumer);
const program = ts.createProgram([consumer], {
  strict: true,
  noEmit: true,
  skipLibCheck: true,
  types: [],
  target: ts.ScriptTarget.ES2022,
  module: ts.ModuleKind.ESNext,
  moduleResolution: ts.ModuleResolutionKind.Bundler,
});
const diagnostics = ts.getPreEmitDiagnostics(program);
assert.equal(
  diagnostics.length,
  0,
  ts.formatDiagnosticsWithColorAndContext(diagnostics, {
    getCanonicalFileName: (file) => file,
    getCurrentDirectory: () => directory,
    getNewLine: () => "\n",
  }),
);
