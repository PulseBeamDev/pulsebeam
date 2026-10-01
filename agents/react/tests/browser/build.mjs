import { build } from "esbuild";
import { cp, mkdir, rm, stat, writeFile } from "node:fs/promises";

const output = process.argv[2] ?? "tests/browser/dist";
const webDist = process.argv[3] ?? "../pulsebeam-agent-web/dist";
await rm(output, { recursive: true, force: true });
await mkdir(output, { recursive: true });
await cp("tests/browser/index.html", `${output}/index.html`);
await cp(
  `${webDist}/wasm/pulsebeam_agent_web_bg.wasm`,
  `${output}/pulsebeam_agent_web_bg.wasm`,
);
await build({
  entryPoints: ["tests/browser/fixture.tsx"],
  bundle: true,
  format: "esm",
  outfile: `${output}/fixture.js`,
  platform: "browser",
  jsx: "automatic",
  alias: { "@pulsebeam/react": "./package/dist/index.js" },
});
const inputs = [
  "tests/browser/fixture.tsx",
  "tests/browser/acquisition.tsx",
  "tests/browser/ownership.tsx",
  "tests/browser/playback.tsx",
  "tests/browser/index.html",
  `${webDist}/index.js`,
];
await writeFile(
  `${output}/fixture-manifest.json`,
  JSON.stringify({
    inputs: await Promise.all(
      inputs.map(async (path) => [path, (await stat(path)).mtimeMs]),
    ),
  }),
);
