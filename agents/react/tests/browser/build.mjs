import { build } from "esbuild";
import { cp, mkdir, rm } from "node:fs/promises";

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
