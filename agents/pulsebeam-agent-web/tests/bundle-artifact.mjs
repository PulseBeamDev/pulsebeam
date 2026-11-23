import esbuild from "../../react/node_modules/esbuild/lib/main.js";

await esbuild.build({
  entryPoints: ["package/dist/index.js"],
  bundle: true,
  platform: "browser",
  format: "esm",
  outfile: "node-sdk.mjs",
});
