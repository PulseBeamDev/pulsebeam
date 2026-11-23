import { build } from "esbuild";
import { cp, mkdir } from "node:fs/promises";
import { resolve } from "node:path";

const [output, entry = "quickstart_source/main.tsx", installed] =
  process.argv.slice(2);
await mkdir(output, { recursive: true });
await cp("quickstart_source/index.html", `${output}/index.html`);
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
            build.onResolve(
              { filter: /^@pulsebeam\/web(?:\/internal\/react)?$/ },
              (args) => {
                if (args.pluginData === "installed-sdk") return;
                return build.resolve(args.path, {
                  kind: args.kind,
                  resolveDir: resolve(installed),
                  pluginData: "installed-sdk",
                });
              },
            );
          },
        },
      ]
    : [],
  alias: installed
    ? undefined
    : {
        "@pulsebeam/react": "./package/dist/index.js",
      },
});
