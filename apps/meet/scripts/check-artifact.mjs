import { existsSync, readFileSync } from "node:fs";
import { createServer } from "node:http";
import { extname, join, normalize } from "node:path";

const out = "out";
for (const file of ["index.html", "CNAME"])
  if (!existsSync(join(out, file))) throw new Error(`Missing exported ${file}`);
if (readFileSync(join(out, "CNAME"), "utf8").trim() !== "meet.pulsebeam.dev")
  throw new Error("Unexpected exported CNAME");
const html = readFileSync(join(out, "index.html"), "utf8");
const modules = [...html.matchAll(/(?:src|href)="([^"]+\.js)"/g)].map(
  (match) => match[1],
);
if (!modules.length) throw new Error("Export must reference module assets");
for (const asset of modules)
  if (!asset.startsWith("/") || !existsSync(join(out, asset)))
    throw new Error(`Invalid exported asset path: ${asset}`);
const wasm = modules.flatMap((module) =>
  [
    ...readFileSync(join(out, module), "utf8").matchAll(
      /(?:"|')([^"']+\.wasm)(?:"|')/g,
    ),
  ].map((match) => match[1]),
);
if (!wasm.length)
  throw new Error("Exported modules must reference destination WASM");
for (const asset of wasm)
  if (!asset.startsWith("/") || !existsSync(join(out, asset)))
    throw new Error(`Invalid exported asset path: ${asset}`);
const server = createServer((request, response) => {
  const target = normalize(join(out, request.url ?? "/"));
  if (!target.startsWith(out) || !existsSync(target)) {
    response.statusCode = 404;
    response.end();
    return;
  }
  response.setHeader(
    "Content-Type",
    extname(target) === ".wasm"
      ? "application/wasm"
      : "application/octet-stream",
  );
  response.end(readFileSync(target));
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
const response = await fetch(`http://127.0.0.1:${port}${wasm[0]}`);
server.close();
if (
  !response.ok ||
  !response.headers.get("content-type")?.startsWith("application/wasm")
)
  throw new Error("Exported WASM is not served with application/wasm");
