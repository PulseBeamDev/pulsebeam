import assert from "node:assert/strict";
import { test } from "node:test";
import { sessionKey } from "../unit-javascript/session-key.js";

const config = { endpoint: "https://example.test", token: "first" };

test("credentials and logging update the existing session", () => {
  const key = sessionKey(config);
  assert.equal(sessionKey({ ...config, token: "renewed" }), key);
  assert.equal(sessionKey({ ...config, logging: { level: "debug" } }), key);
});

test("endpoint is construction identity", () => {
  assert.notEqual(
    sessionKey({ ...config, endpoint: "https://other.test" }),
    sessionKey(config),
  );
});
