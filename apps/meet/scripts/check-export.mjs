import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

assert.ok((await readFile("out/index.html", "utf8")).trim().length > 0);
assert.equal(
  (await readFile("out/CNAME", "utf8")).trim(),
  "meet.pulsebeam.dev",
);
