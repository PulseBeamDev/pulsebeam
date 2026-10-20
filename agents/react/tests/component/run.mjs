import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { resolve, join } from "node:path";
import { pathToFileURL } from "node:url";

const owner = process.cwd();
const root = resolve(owner, "../..");
const require = createRequire(join(owner, "package.json"));
const { build } = await import(pathToFileURL(require.resolve("esbuild")).href);
const { Window } = await import(
  pathToFileURL(require.resolve("happy-dom")).href
);
const window = new Window({ url: "http://localhost/" });
for (const name of [
  "window",
  "document",
  "navigator",
  "Node",
  "Element",
  "DocumentFragment",
  "ShadowRoot",
  "HTMLElement",
  "HTMLMediaElement",
  "HTMLVideoElement",
  "HTMLInputElement",
  "HTMLTextAreaElement",
  "Event",
  "EventTarget",
  "MouseEvent",
  "KeyboardEvent",
  "MutationObserver",
  "ResizeObserver",
  "Storage",
  "Document",
  "localStorage",
  "sessionStorage",
  "location",
  "history",
  "getComputedStyle",
  "requestAnimationFrame",
  "cancelAnimationFrame",
]) {
  const value = name === "window" ? window : window[name];
  Object.defineProperty(globalThis, name, {
    configurable: true,
    value:
      typeof value === "function" && /^[a-z]/.test(name)
        ? value.bind(window)
        : value,
  });
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
const output = join(process.env.TEST_TMPDIR, "components.mjs");
await build({
  entryPoints: [resolve(process.argv[2])],
  outfile: output,
  bundle: true,
  platform: "node",
  format: "esm",
  banner: {
    js: 'import { createRequire } from "node:module"; const require = createRequire(import.meta.url);',
  },
  jsx: "automatic",
  tsconfigRaw: {
    compilerOptions: { baseUrl: owner, paths: { "@/*": ["./*"] } },
  },
  alias: {
    "@pulsebeam/react": join(root, "agents/react/src/index.ts"),
    "@pulsebeam/web/internal/react": join(
      root,
      "agents/pulsebeam-agent-web/web/internal/react.ts",
    ),
    "@pulsebeam/web": join(root, "agents/pulsebeam-agent-web/web/index.ts"),
  },
  plugins: [
    {
      name: "external-platform",
      setup(build) {
        build.onResolve({ filter: /^react(?:-dom)?(?:\/|$)/ }, (args) => ({
          path: require.resolve(args.path),
          external: true,
        }));
        build.onResolve({ filter: /(?:wasm\.js|runtime(?:\.js)?)$/ }, (args) =>
          (args.path === "./wasm.js" &&
            args.resolveDir.endsWith("/pulsebeam-agent-web/web")) ||
          args.resolveDir.endsWith("/tests/component") ||
          args.path.endsWith("/tests/component/runtime")
            ? {
                path: join(root, "agents/react/tests/component/runtime.ts"),
                namespace: "runtime-model",
              }
            : undefined,
        );
        build.onLoad({ filter: /.*/, namespace: "runtime-model" }, (args) => ({
          contents: readFileSync(args.path, "utf8"),
          loader: "ts",
        }));
      },
    },
  ],
});
try {
  await import(pathToFileURL(output).href);
} finally {
  await window.happyDOM.close();
}
