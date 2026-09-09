import { build } from "esbuild";
import { cp, mkdir, rm, stat, writeFile } from "node:fs/promises";

await rm("tests/browser/dist", { recursive: true, force: true });
await mkdir("tests/browser/dist", { recursive: true });
await cp("tests/browser/index.html", "tests/browser/dist/index.html");
await cp(
  "../pulsebeam-agent-web/dist/wasm/pulsebeam_agent_web_bg.wasm",
  "tests/browser/dist/pulsebeam_agent_web_bg.wasm",
);
await build({
  entryPoints: ["tests/browser/fixture.tsx"],
  bundle: true,
  format: "esm",
  outfile: "tests/browser/dist/fixture.js",
  platform: "browser",
  jsx: "automatic",
});
const inputs = [
  "tests/browser/fixture.tsx",
  "tests/browser/index.html",
  "../pulsebeam-agent-web/dist/index.js",
];
await writeFile(
  "tests/browser/dist/fixture-manifest.json",
  JSON.stringify({
    inputs: await Promise.all(
      inputs.map(async (path) => [path, (await stat(path)).mtimeMs]),
    ),
  }),
);
