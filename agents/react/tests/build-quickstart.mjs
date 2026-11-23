import { build } from "esbuild";
import { cp, mkdir } from "node:fs/promises";
import { resolve } from "node:path";

const [output, entry = "quickstart_source/main.tsx", installed] =
  process.argv.slice(2);
await mkdir(output, { recursive: true });
await cp("quickstart_source/index.html", `${output}/index.html`);
if (!installed) {
  await cp(
    "../pulsebeam-agent-web/package/dist/wasm/pulsebeam_agent_web_bg.wasm",
    `${output}/pulsebeam_agent_web_bg.wasm`,
  );
}
await build({
  entryPoints: [entry],
  bundle: true,
  format: "esm",
  outfile: `${output}/main.js`,
  platform: "browser",
  jsx: "automatic",
  plugins: installed
    ? [
        {
          name: "installed-sdk",
          setup(build) {
            build.onResolve({ filter: /^@pulsebeam\/web$/ }, (args) => {
              if (args.pluginData === "installed-sdk") return;
              return build.resolve(args.path, {
                kind: args.kind,
                resolveDir: resolve(installed),
                pluginData: "installed-sdk",
              });
            });
          },
        },
      ]
    : [],
  alias: installed
    ? undefined
    : {
        "@pulsebeam/react": "./package/dist/index.js",
        "@pulsebeam/web/internal/react":
          "../pulsebeam-agent-web/package/dist/internal/react.js",
        "@pulsebeam/web": "../pulsebeam-agent-web/package/dist/index.js",
      },
});
