import { build } from "esbuild";
import { cp, mkdir } from "node:fs/promises";

const output = process.argv[2];
await mkdir(output, { recursive: true });
await cp("quickstart_source/index.html", `${output}/index.html`);
await cp(
  "../pulsebeam-agent-web/package/dist/wasm/pulsebeam_agent_web_bg.wasm",
  `${output}/pulsebeam_agent_web_bg.wasm`,
);
await build({
  entryPoints: ["quickstart_source/main.tsx"],
  bundle: true,
  format: "esm",
  outfile: `${output}/main.js`,
  platform: "browser",
  jsx: "automatic",
  alias: {
    "@pulsebeam/react": "./package/dist/index.js",
    "@pulsebeam/web": "../pulsebeam-agent-web/package/dist/index.js",
  },
});
