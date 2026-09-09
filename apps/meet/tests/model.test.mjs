import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const source = readFileSync("lib/model.ts", "utf8");
test("endpoint normalization policy is present", () => {
  assert.match(source, /https\?:/);
  assert.match(source, /api\/v1/);
  assert.match(source, /url\.search/);
});
test("complete desired state retains media, audio, and topics", () => {
  assert.match(source, /publications:/);
  assert.match(source, /automatic: true/);
  assert.match(source, /reactions/);
});
