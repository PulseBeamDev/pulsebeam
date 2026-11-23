import { createRequire } from "node:module";
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
  "Window",
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
  sourcemap: "linked",
  banner: {
    js: 'import { createRequire } from "node:module"; const require = createRequire(import.meta.url);',
  },
  jsx: "automatic",
  tsconfigRaw: {
    compilerOptions: { baseUrl: owner, paths: { "@/*": ["./*"] } },
  },
  alias: {
    "@pulsebeam/react": join(root, "agents/react/src/index.ts"),
  },
  plugins: [
    {
      name: "external-platform",
      setup(build) {
        build.onResolve({ filter: /^react(?:-dom)?(?:\/|$)/ }, (args) => ({
          path: require.resolve(args.path),
          external: true,
        }));
        build.onResolve(
          { filter: /^@pulsebeam\/web(?:\/internal\/react)?$/ },
          (args) => {
            if (args.pluginData === "shared-web") return;
            return build.resolve(args.path, {
              kind: args.kind,
              resolveDir: owner,
              pluginData: "shared-web",
            });
          },
        );
      },
    },
  ],
});
try {
  process.setSourceMapsEnabled(true);
  await import(pathToFileURL(output).href);
} finally {
  await window.happyDOM.close();
}
