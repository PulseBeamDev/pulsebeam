import assert from "node:assert/strict";
import { cpSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
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
writeFileSync(
  join(directory, "consumer.mjs"),
  'export * from "@pulsebeam/react";',
);
const sdk = await import(pathToFileURL(join(directory, "consumer.mjs")));
for (const name of [
  "useAgent",
  "useDevices",
  "useCapture",
  "Publish",
  "Data",
  "Video",
  "Audio",
]) {
  assert.equal(typeof sdk[name], "function", name);
}
assert.equal(typeof sdk.track, "object", "track");
assert.equal(
  "createAgent" in sdk,
  false,
  "React has only hook-owned Agent construction",
);
const web = await import(
  pathToFileURL(join(directory, "node_modules/@pulsebeam/web/dist/index.js"))
);
assert.equal(typeof web.createAgent, "function");
assert.equal(
  "createManagedAgent" in web,
  false,
  "managed lifecycle is not a public entrypoint",
);
assert.equal("createWebAgent" in web, false);
const wasm = readFileSync(
  join(
    directory,
    "node_modules/@pulsebeam/web/dist/wasm/pulsebeam_agent_web_bg.wasm",
  ),
);
await WebAssembly.compile(wasm);
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
