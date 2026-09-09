import { build } from "esbuild";
import { cp, mkdir } from "node:fs/promises";

await mkdir("tests/browser/dist", { recursive: true });
await cp("tests/browser/index.html", "tests/browser/dist/index.html");
await build({ entryPoints: ["tests/browser/fixture.tsx"], bundle: true, format: "esm", outfile: "tests/browser/dist/fixture.js", platform: "browser" });
